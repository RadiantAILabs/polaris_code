//! MCP clients over stdio and Streamable HTTP.
//!
//! [`McpClientPlugin`] discovers tools before the framework freezes its tool
//! registry. Tool execution uses the existing approval, cancellation, redaction,
//! and trajectory paths; server-provided annotations never grant permissions.
//! See the [MCP guide](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/mcp.md)
//! for configuration and the external-server trust boundary.

mod config;
mod http;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use polaris::models::llm::ToolDefinition;
use polaris::system::plugin::{
    Contract, DefaultDependencies, Plugin, PluginAccess, PluginId, Version, VersionReq,
};
use polaris::system::resource::GlobalResource;
use polaris::system::server::Server;
use polaris::tools::{Tool, ToolContext, ToolError, ToolPermission, ToolRegistry, ToolsPlugin};
use polaris_tui::commands::CommandSpec;
use rmcp::model::{CallToolRequestParams, PaginatedRequestParams};
use rmcp::service::{Peer, RunningService, RunningServiceCancellationToken};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::project_trust::{ProjectTrust, Subject};
use crate::terminal::{CommandOutcome, CommandRegistry};
use crate::tools::{
    AttendedApproval, ToolEffect, ToolPolicy, ToolPolicyPlugin, ToolPolicyRegistry,
};
use config::{ServerConfig, valid_component};

/// Prefix reserved for dynamically discovered MCP tools.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

struct Connection {
    peer: Peer<RoleClient>,
    service: Mutex<RunningService<RoleClient, ()>>,
    timeout: Duration,
}

impl Connection {
    async fn close(&self) {
        let _ = self
            .service
            .lock()
            .await
            .close_with_timeout(SHUTDOWN_TIMEOUT)
            .await;
    }
}

struct ServerEntry {
    name: String,
    read_only: bool,
    tools: Vec<McpTool>,
    connection: Option<Arc<Connection>>,
    status: McpStatus,
}

/// Connection state of a configured MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum McpStatus {
    /// Initialization and tool discovery succeeded.
    Connected,
    /// A previously connected server has closed its transport.
    Disconnected,
    /// Configuration explicitly disables this server.
    Disabled,
    /// A project server was declared but not started because the user has not
    /// trusted its declaration.
    Untrusted,
    /// Initialization or discovery failed, with a sanitized reason.
    Failed(String),
}

impl fmt::Display for McpStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connected => f.write_str("connected"),
            Self::Disconnected => f.write_str("disconnected"),
            Self::Disabled => f.write_str("disabled"),
            Self::Untrusted => f.write_str("not started: project server not trusted"),
            Self::Failed(reason) => f.write_str(reason),
        }
    }
}

/// A snapshot of one configured MCP server, with current connection status.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct McpServerStatus {
    /// Operator-configured server name.
    pub name: String,
    /// Connection state, a project server waiting for trust, or a bounded
    /// startup failure.
    pub status: McpStatus,
    /// Whether the user's own configuration declared this server read-only. A
    /// project's `readOnly` claim is never honored.
    pub read_only: bool,
    /// Registered tool names, including the server namespace.
    pub tools: Vec<String>,
}

/// Process-wide MCP connections and discovery results.
///
/// Provided globally by [`McpClientPlugin`]. Consumers read [`servers`](Self::servers)
/// for status without reconnecting or executing tools. `/mcp` uses this same
/// projection. Connections live until plugin cleanup, with bounded shutdown.
///
/// # Scope
///
/// Global: server connections belong to the application process and are shared
/// across turns and sessions.
///
/// # Provided By
///
/// [`McpClientPlugin`] registers this resource during `build`.
///
/// # Access
///
/// Read with `Res<McpRegistry>`. Configuration and discovered tools are immutable;
/// connection health is read from the SDK peer, and shutdown uses an internal
/// asynchronous mutex. Consumers cannot mutate connections through this API.
///
/// # Alternatives
///
/// [`ToolRegistry`] exposes tool definitions and execution without connection
/// status. No other resource exposes configured MCP server health.
///
/// # Example
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_code::tools::mcp::McpRegistry;
///
/// #[system]
/// async fn inspect_mcp(registry: Res<McpRegistry>) {
///     for server in registry.servers() {
///         tracing::info!(server = server.name, status = %server.status, "MCP status");
///     }
/// }
/// ```
#[derive(Clone, Default)]
pub struct McpRegistry {
    entries: Arc<Vec<ServerEntry>>,
    errors: Arc<Vec<String>>,
}

impl fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRegistry")
            .field("servers", &self.servers())
            .field("configuration_errors", &self.configuration_errors())
            .finish()
    }
}

impl GlobalResource for McpRegistry {}

impl Contract for McpRegistry {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl McpRegistry {
    /// Configured servers in name order; connection health is checked at read time.
    #[must_use]
    pub fn servers(&self) -> Vec<McpServerStatus> {
        self.entries
            .iter()
            .map(|entry| McpServerStatus {
                name: entry.name.clone(),
                status: match &entry.connection {
                    Some(connection) if connection.peer.is_transport_closed() => {
                        McpStatus::Disconnected
                    }
                    _ => entry.status.clone(),
                },
                read_only: entry.read_only,
                tools: entry
                    .tools
                    .iter()
                    .map(|tool| tool.definition.name.clone())
                    .collect(),
            })
            .collect()
    }

    /// Configuration failures without credential values or raw server responses.
    #[must_use]
    pub fn configuration_errors(&self) -> &[String] {
        &self.errors
    }

    // Record project servers that were declared but not started. They have no
    // connection and no tools.
    fn with_untrusted(mut self, names: impl IntoIterator<Item = String>) -> Self {
        if let Some(entries) = Arc::get_mut(&mut self.entries) {
            entries.extend(names.into_iter().map(|name| ServerEntry {
                name,
                read_only: false,
                tools: Vec::new(),
                connection: None,
                status: McpStatus::Untrusted,
            }));
            entries.sort_by(|a, b| a.name.cmp(&b.name));
        }
        self
    }

    fn untrusted(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.status == McpStatus::Untrusted)
            .map(|entry| entry.name.as_str())
            .collect()
    }

    fn report(&self) -> String {
        let mut report = String::new();
        for error in self.configuration_errors() {
            report.push_str(&format!("{error}\n\n"));
        }
        for server in self.servers() {
            if server.status == McpStatus::Untrusted {
                report.push_str(&format!(
                    "- `{}`: {}; review it with `/mcp trust`\n",
                    server.name, server.status
                ));
                continue;
            }
            let kind = if server.read_only {
                "read-only"
            } else {
                "untrusted"
            };
            report.push_str(&format!(
                "- `{}`: {} ({kind}); {} tool(s)\n",
                server.name,
                server.status,
                server.tools.len()
            ));
            for tool in server.tools {
                report.push_str(&format!("  - `{tool}`\n"));
            }
        }
        if report.is_empty() {
            report.push_str("No MCP servers configured. Add .mcp.json or a user polaris/mcp.json.");
        }
        report
    }
}

#[derive(Clone)]
struct McpTool {
    definition: ToolDefinition,
    remote_name: String,
    read_only: bool,
    connection: Arc<Connection>,
}

// Dropping an in-flight call (turn cancellation or timeout) closes the
// connection. A stdio server is then terminated rather than left performing
// an abandoned call. HTTP cancellation cannot undo remote side effects.
struct CallGuard(Option<RunningServiceCancellationToken>);

impl Drop for CallGuard {
    fn drop(&mut self) {
        if let Some(token) = self.0.take() {
            token.cancel();
        }
    }
}

fn budget_result(text: String) -> String {
    let limit = super::code::MAX_READ_BYTES;
    if text.len() <= limit {
        return text;
    }
    let mut cut = limit;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n… (MCP result truncated at {limit} bytes)",
        &text[..cut]
    )
}

impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    fn permission(&self) -> ToolPermission {
        // Confirm for everyone. What an operator's `readOnly` declaration
        // allows is published as a policy fact in `build`, never as `Allow`.
        ToolPermission::Confirm
    }

    fn execute<'ctx>(
        &'ctx self,
        args: Value,
        _ctx: &'ctx ToolContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'ctx>> {
        Box::pin(async move {
            let Value::Object(arguments) = args else {
                return Err(ToolError::parameter_error(
                    "MCP arguments must be an object",
                ));
            };
            let request =
                CallToolRequestParams::new(self.remote_name.clone()).with_arguments(arguments);
            let mut guard = CallGuard(Some(
                self.connection.service.lock().await.cancellation_token(),
            ));
            let result = tokio::time::timeout(
                self.connection.timeout,
                self.connection.peer.call_tool(request),
            )
            .await
            .map_err(|_| ToolError::execution_error("MCP tool call timed out"))?;
            guard.0 = None;
            let result = result.map_err(|_| {
                ToolError::execution_error("MCP server request failed or disconnected")
            })?;
            // Preserve all response kinds as JSON, including structuredContent
            // and resource links. Inline media is subject to the same byte cap.
            let text = serde_json::to_string(&result)
                .map_err(|_| ToolError::execution_error("Cannot serialize MCP tool result"))?;
            let text = budget_result(text);
            if result.is_error == Some(true) {
                Err(ToolError::execution_error(text))
            } else {
                Ok(Value::String(text))
            }
        })
    }
}

async fn connect(
    config: &ServerConfig,
    working_dir: &Path,
) -> Result<Arc<Connection>, &'static str> {
    if !(1..=300).contains(&config.timeout_secs) {
        return Err("timeoutSecs must be between 1 and 300");
    }
    let timeout = Duration::from_secs(config.timeout_secs);
    let service = match (&config.command, &config.url) {
        (Some(executable), None) if !executable.is_empty() => {
            let mut command = tokio::process::Command::new(executable);
            command
                .args(&config.args)
                .current_dir(working_dir)
                .env_clear()
                .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                .kill_on_drop(true);
            // Match the shell's small non-credential base. Credential-shaped
            // host variables are never inherited; explicit env is opt-in.
            for name in ["HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR"] {
                if let Some(value) = std::env::var_os(name) {
                    command.env(name, value);
                }
            }
            command.envs(&config.env);
            let mut command = process_wrap::tokio::CommandWrap::from(command);
            #[cfg(unix)]
            command.wrap(process_wrap::tokio::ProcessGroup::leader());
            #[cfg(windows)]
            command
                .wrap(process_wrap::tokio::KillOnDrop)
                .wrap(process_wrap::tokio::JobObject);
            // Server stderr cannot bypass redaction or write into the TUI.
            let (transport, _) = TokioChildProcess::builder(command)
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| "cannot start server command")?;
            tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| "initialization timed out")?
                .map_err(|_| "MCP initialization failed")?
        }
        (None, Some(url)) if url.starts_with("https://") || url.starts_with("http://") => {
            let client = http::BoundedHttpClient::new().map_err(|_| "cannot build HTTP client")?;
            let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone());
            transport_config.max_sse_event_size = http::MAX_HTTP_BYTES;
            let transport = StreamableHttpClientTransport::with_client(client, transport_config);
            tokio::time::timeout(timeout, ().serve(transport))
                .await
                .map_err(|_| "initialization timed out")?
                .map_err(|_| "MCP initialization failed")?
        }
        _ => return Err("set exactly one command or HTTP(S) url"),
    };
    Ok(Arc::new(Connection {
        peer: service.peer().clone(),
        service: Mutex::new(service),
        timeout,
    }))
}

struct DiscoveryBudget(usize);

impl std::io::Write for DiscoveryBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > http::MAX_HTTP_BYTES - self.0 {
            return Err(std::io::Error::other("discovery metadata limit exceeded"));
        }
        self.0 += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn list_tools(connection: &Connection) -> Result<Vec<rmcp::model::Tool>, &'static str> {
    let mut tools = Vec::new();
    let mut cursor = None;
    let mut metadata = DiscoveryBudget(0);
    let mut cursors = std::collections::HashSet::new();
    // Bound both the total number of tools and the number of empty pages.
    for _ in 0..32 {
        let page = connection
            .peer
            .list_tools(Some(PaginatedRequestParams::default().with_cursor(cursor)))
            .await
            .map_err(|_| "tool discovery failed")?;
        if tools.len() + page.tools.len() > 256 {
            return Err("tool discovery exceeded 256 tools");
        }
        // Count serialized metadata without making another copy. The budget
        // covers schemas, descriptions, annotations, and names across pages.
        if serde_json::to_writer(&mut metadata, &page.tools).is_err() {
            return Err("tool discovery exceeded 1048576 metadata bytes");
        }
        tools.extend(page.tools);
        cursor = page.next_cursor;
        match &cursor {
            None => return Ok(tools),
            Some(value) if !cursors.insert(value.clone()) => {
                return Err("tool discovery repeated a cursor");
            }
            Some(_) => {}
        }
    }
    Err("tool discovery exceeded 32 pages")
}

async fn discover(
    configs: BTreeMap<String, ServerConfig>,
    errors: Vec<String>,
    working_dir: &Path,
) -> McpRegistry {
    if configs.len() > 32 {
        let mut errors = errors;
        errors.push("MCP configuration exceeds 32 servers; no servers started".into());
        return McpRegistry {
            entries: Arc::new(Vec::new()),
            errors: Arc::new(errors),
        };
    }
    let mut entries = Vec::new();
    for (name, config) in configs {
        let mut entry = ServerEntry {
            name: name.clone(),
            read_only: config.read_only,
            tools: Vec::new(),
            connection: None,
            status: McpStatus::Disabled,
        };
        if !valid_component(&name) {
            // Do not display arbitrary untrusted server names in terminal text.
            entry.name = "(invalid server name)".into();
            entry.status =
                McpStatus::Failed("use letters, digits, single underscores, or hyphens".into());
        } else if !config.disabled {
            match connect(&config, working_dir).await {
                Err(error) => entry.status = McpStatus::Failed(error.into()),
                Ok(connection) => {
                    match tokio::time::timeout(connection.timeout, list_tools(&connection)).await {
                        Ok(Ok(tools)) => {
                            let mut names = std::collections::HashSet::new();
                            let valid = tools.iter().all(|tool| {
                                valid_component(&tool.name)
                                    && format!("{MCP_TOOL_PREFIX}{name}__{}", tool.name).len() <= 64
                                    && names.insert(tool.name.to_string())
                            });
                            if valid {
                                entry.tools = tools
                                    .into_iter()
                                    .map(|tool| McpTool {
                                        definition: ToolDefinition::new(
                                            format!("{MCP_TOOL_PREFIX}{name}__{}", tool.name),
                                            tool.description.unwrap_or_default().into_owned(),
                                            Value::Object((*tool.input_schema).clone()),
                                        )
                                        .with_strict(false),
                                        remote_name: tool.name.into_owned(),
                                        read_only: config.read_only,
                                        connection: connection.clone(),
                                    })
                                    .collect();
                                entry.status = McpStatus::Connected;
                                entry.connection = Some(connection);
                            } else {
                                entry.status = McpStatus::Failed(
                                    "discovery rejected invalid or duplicate tool names".into(),
                                );
                                connection.close().await;
                            }
                        }
                        failure => {
                            let reason = match failure {
                                Ok(Err(reason)) => reason,
                                Err(_) => "tool discovery timed out",
                                Ok(Ok(_)) => unreachable!(),
                            };
                            entry.status = McpStatus::Failed(reason.into());
                            connection.close().await;
                        }
                    }
                }
            }
        }
        entries.push(entry);
    }
    McpRegistry {
        entries: Arc::new(entries),
        errors: Arc::new(errors),
    }
}

/// Discover configured MCP servers and register their tools with Polaris.
///
/// Add this plugin when an assembly should expose configured external tools.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`McpRegistry`] | Global | Server health, discovered tools, and sanitized configuration failures |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Extends the upstream tool registry and registers `/mcp` on the supplied [`CommandRegistry`]; provides no API of its own. |
///
/// # Dependencies
///
/// Requires [`ToolsPlugin`] and extends its [`ToolRegistry`] capability. It also
/// extends the [`ToolPolicyRegistry`] provided by [`ToolPolicyPlugin`], which is
/// added automatically when missing.
///
/// # Tools Provided
///
/// | Tool | Permission | Description |
/// |------|------------|-------------|
/// | `mcp__<server>__<tool>` | `Confirm` | One dynamic adapter for each discovered MCP tool |
///
/// # Permissions
///
/// Every tool registers as [`ToolPermission::Confirm`]. The plugin declares two
/// policy facts for each tool: every attended call needs a person
/// ([`AttendedApproval::Human`]), and the tool is [`ToolEffect::ReadOnly`] only
/// when the user's own configuration declared its server `readOnly` (otherwise
/// [`ToolEffect::Mutating`]). Agent policy turns those facts into behaviour:
///
/// | Mode | Result |
/// |------|--------|
/// | `confirm`, `auto-edit`, `auto` | Asks a person. Allow rules, session grants and the automatic reviewer never settle it. |
/// | `plan` | Runs a read-only server's tools without asking; refuses every other server's. |
/// | Unattended | Refused unless the run grants the exact tool name. |
///
/// Remote annotations do not grant permission. External servers are outside the
/// local edit journal and filesystem containment boundary; see the
/// [MCP guide](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/mcp.md).
///
/// Starting a server is a separate decision that comes first: a project's
/// servers start only once the user has trusted their exact declarations, in
/// every permission mode and in unattended runs.
///
/// # Lifecycle
///
/// Construction performs bounded process/network initialization and discovery.
/// `build` publishes the tools before the registry freezes. Cleanup closes every
/// connection with a bounded wait and terminates stdio children. HTTP input is
/// limited to 1 MiB before JSON/SSE parsing, and discovery retains at most 1 MiB
/// of serialized tool metadata across all pages.
///
/// # Example
///
/// ```no_run
/// # async fn example(server: &mut polaris::system::server::Server) {
/// use std::sync::Arc;
/// use polaris_code::project_trust::ProjectTrust;
/// use polaris_code::terminal::CommandRegistry;
/// use polaris_code::tools::mcp::McpClientPlugin;
/// let commands = Arc::new(CommandRegistry::with_builtins());
/// let project = std::path::Path::new(".");
/// server.add_plugins(polaris::tools::ToolsPlugin);
/// server.add_plugins(
///     McpClientPlugin::from_working_dir(project, commands, ProjectTrust::for_user(project)).await,
/// );
/// # }
/// ```
#[derive(Debug)]
pub struct McpClientPlugin {
    registry: McpRegistry,
}

impl McpClientPlugin {
    /// Load user and project JSON, connect, and discover tools.
    ///
    /// The user's own servers always start. A project's `.mcp.json` runs
    /// commands from the repository, so a project server starts only when
    /// `trust` holds the user's decision for its exact declaration; others are
    /// listed by `/mcp` and never started or connected to. A project server
    /// never replaces a user server of the same name, and its `readOnly` claim
    /// is ignored. `/mcp trust <server>` records a decision, which applies the
    /// next time Polaris starts.
    ///
    /// Failures remain visible in `/mcp`; they do not prevent other servers or
    /// the rest of the application from starting.
    pub async fn from_working_dir(
        working_dir: &Path,
        commands: Arc<CommandRegistry>,
        trust: ProjectTrust,
    ) -> Self {
        let user_path = crate::config_dir::config_file("mcp.json");
        Self::start(working_dir, user_path.as_deref(), commands, trust).await
    }

    async fn start(
        working_dir: &Path,
        user_path: Option<&Path>,
        commands: Arc<CommandRegistry>,
        trust: ProjectTrust,
    ) -> Self {
        let loaded = config::load(working_dir, user_path, &trust);
        let summaries: Arc<Vec<(String, String)>> = Arc::new(
            loaded
                .withheld
                .iter()
                .map(|server| (server.name.clone(), server.summary()))
                .collect(),
        );
        let registry = discover(loaded.servers, loaded.errors, working_dir)
            .await
            .with_untrusted(summaries.iter().map(|(name, _)| name.clone()));
        let projection = registry.clone();
        if commands
            .register_sync(
                CommandSpec::new("mcp", "List MCP servers, connection status, and tools")
                    .with_usage("[trust [server]]")
                    .with_choices(["trust"]),
                move |_, args| run_command(args, &projection, &summaries, &trust),
            )
            .is_err()
        {
            tracing::warn!("Could not register /mcp: command already exists");
        }
        Self { registry }
    }

    /// What the person running Polaris should be told once startup is done.
    ///
    /// Names the project servers that were not started because they are not
    /// trusted, and how to trust them. Empty when nothing was held back. A
    /// front end shows this once: on stderr for an unattended run, in the
    /// transcript for a session.
    #[must_use]
    pub fn startup_notes(&self) -> Vec<String> {
        let names = self.registry.untrusted();
        if names.is_empty() {
            return Vec::new();
        }
        let names = names
            .iter()
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>()
            .join(", ");
        vec![format!(
            "Project MCP servers not started: {names}. They run commands declared in this \
             repository. Review them with `/mcp trust` in an interactive session; a decision \
             also applies to later unattended runs until the declaration changes."
        )]
    }
}

/// `/mcp`, `/mcp trust` and `/mcp trust <server>`.
fn run_command(
    args: &str,
    registry: &McpRegistry,
    summaries: &[(String, String)],
    trust: &ProjectTrust,
) -> CommandOutcome {
    let words = args.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        [] => CommandOutcome::Reply(registry.report()),
        ["trust"] if summaries.is_empty() => {
            CommandOutcome::Reply("No project MCP servers are waiting for your trust.".into())
        }
        ["trust"] => {
            let mut text = String::from(
                "Project MCP servers waiting for your trust. A trusted server runs with your \
                 account's access, so read what it would do first.\n",
            );
            for (name, summary) in summaries {
                text.push_str(&format!(
                    "- `{name}` {summary}\n  Trust it: `/mcp trust {name}`\n"
                ));
            }
            text.push_str(
                "A trusted server starts the next time Polaris starts, and stays trusted until \
                 its declaration changes.",
            );
            CommandOutcome::Reply(text)
        }
        ["trust", name] if registry.untrusted().contains(name) => {
            match trust.grant_observed(&Subject::McpServer((*name).to_string())) {
                Ok(_) => CommandOutcome::Reply(format!(
                    "Trusted project MCP server `{name}`. It starts the next time Polaris starts."
                )),
                Err(error) => {
                    CommandOutcome::Error(format!("Could not save trust for `{name}`: {error}"))
                }
            }
        }
        ["trust", name] => CommandOutcome::Error(format!(
            "`{name}` is not a project MCP server waiting for trust; run `/mcp trust` to list them."
        )),
        _ => CommandOutcome::Error("Usage: /mcp [trust [server]]".into()),
    }
}

impl Plugin for McpClientPlugin {
    const ID: &'static str = "polaris_code::mcp_client";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<ToolsPlugin>(),
            PluginId::of::<ToolPolicyPlugin>(),
        ]
    }

    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new().add::<ToolPolicyPlugin>()
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
            .extends::<ToolPolicyRegistry>(VersionReq::caret(ToolPolicyRegistry::CONTRACT_VERSION))
            .provides::<McpRegistry>(McpRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before McpClientPlugin");
        let mut policies = server
            .get_resource_mut::<ToolPolicyRegistry>()
            .expect("ToolPolicyPlugin must be added before McpClientPlugin");
        for entry in self.registry.entries.iter() {
            for tool in &entry.tools {
                registry.register(tool.clone());
                // Only the operator's declaration can call a server read-only;
                // a remote `readOnlyHint` is never read. Everything else may
                // change state, and every attended call asks a person.
                policies.declare(
                    tool.definition.name.clone(),
                    ToolPolicy::default()
                        .with_effect(if tool.read_only {
                            ToolEffect::ReadOnly
                        } else {
                            ToolEffect::Mutating
                        })
                        .with_attended(AttendedApproval::Human),
                );
            }
        }
        drop(registry);
        drop(policies);
        server.insert_global(self.registry.clone());
    }

    async fn cleanup(&self, _server: &mut Server) {
        for entry in self.registry.entries.iter() {
            if let Some(connection) = &entry.connection {
                connection.close().await;
            }
        }
    }
}
