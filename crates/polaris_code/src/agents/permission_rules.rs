//! Argument-aware permission rules and host-owned approval scopes.
//!
//! [`PermissionRulesPlugin`] loads immutable user/project rules. Project allow
//! rules require `/permissions trust-project`; restrictive rules apply immediately.
//! The authorization boundary is [`super::PermissionPolicy::resolve_call`]: the
//! automatic reviewer may handle a remaining `Confirm` before the host asks the
//! human, unless the decision is [`ReviewEligibility::HumanOnly`].

mod approval;
mod rules;
mod tool_grants;

pub(crate) use crate::tools::approval::{ApprovalReceipt, call_directory};
pub(crate) use approval::ceiling_permission;
pub use approval::{ApprovalError, ApprovalRequest, ApprovalScope, SessionGrants, ShellApproval};
pub use rules::{PermissionRule, PermissionRules, RuleDecision, RuleSnapshot};
pub use tool_grants::{RunToolGrants, ToolGrantError};

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use polaris::shell::ShellExecutor;
use polaris::system::plugin::{DefaultDependencies, Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris_tui::commands::CommandSpec;

use crate::project_trust::ProjectTrust;
use crate::terminal::{CommandOutcome, CommandRegistry, SessionSettings};
use crate::tools::ToolPolicyPlugin;

/// Loads permission files and provides rule and approval resources.
///
/// Add `PermissionRulesPlugin::new(root)` when an assembly should honour the
/// user's and the repository's permission files. A host that adds nothing, or
/// that adds [`PermissionRulesPlugin::default`], still gets every resource
/// below, but **loads no rule files**: no rule allows, asks or denies anything,
/// so an embedding that omits `new(root)` also omits the deny rules its operator
/// wrote. [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin) installs that
/// disabled default when this plugin is missing.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`PermissionRules`] | Global | Immutable parsed rules, source paths and errors, and the project-trust decision |
/// | [`SessionGrants`] | Global | Human-approved command scopes for the current settings generation |
/// | [`ShellApproval`] | Global | Exact-call authorization and confirmed dispatch over the shell executor |
/// | [`RunToolGrants`] | Global | Immutable additional unattended tool grants |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | Registers the `/permissions` command on a supplied [`CommandRegistry`]; provides no framework API. |
///
/// # Dependencies
///
/// Requires [`ToolPolicyPlugin`], whose facts about each tool feed authorization;
/// it is added automatically when missing. `ready` optionally binds the executor
/// published by [`polaris::shell::ShellPlugin`]. The disabled default loads no
/// files, preserving non-terminal embeddings and hosts without shell tools.
///
/// # Lifecycle
///
/// `build` reads files once, registers `/permissions` when given a command
/// registry and publishes resources. `ready` binds the existing shell executor
/// when one is installed. It starts no tasks or shell processes.
/// Invalid files prevent tool authorization; diagnostics remain inspectable.
///
/// # Example
///
/// ```no_run
/// use std::sync::{Arc, OnceLock};
/// use polaris::system::server::Server;
/// use polaris_code::agents::permission_rules::PermissionRulesPlugin;
/// use polaris_code::terminal::CommandRegistry;
///
/// let commands = Arc::new(CommandRegistry::with_builtins());
/// // The host fills this cell with its `SessionSettings` once they exist.
/// let settings = Arc::new(OnceLock::new());
/// let mut server = Server::new();
/// server.add_plugins(
///     PermissionRulesPlugin::new(".").with_commands(commands, settings),
/// );
/// ```
#[derive(Default)]
pub struct PermissionRulesPlugin {
    root: Option<PathBuf>,
    commands: Option<Arc<CommandRegistry>>,
    settings: Option<Arc<OnceLock<SessionSettings>>>,
    tools: RunToolGrants,
    trust: Option<ProjectTrust>,
}

impl std::fmt::Debug for PermissionRulesPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PermissionRulesPlugin")
            .field("root", &self.root)
            .field("commands", &self.commands.is_some())
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl PermissionRulesPlugin {
    /// Load user rules and `polaris.permissions.toml` at the repository root.
    #[must_use]
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: Some(root.as_ref().to_path_buf()),
            ..Self::default()
        }
    }

    /// Configure already validated launch-time unattended tool grants.
    #[must_use]
    pub fn with_tool_grants(mut self, tools: RunToolGrants) -> Self {
        self.tools = tools;
        self
    }

    /// Keep the user's trust in the project's rule file across sessions.
    ///
    /// Without this, `/permissions trust-project` lasts only for the process.
    #[must_use]
    pub fn with_project_trust(mut self, trust: ProjectTrust) -> Self {
        self.trust = Some(trust);
        self
    }

    /// Advertise local rule inspection and explicit project trust commands.
    ///
    /// `settings` is a late-binding cell: the host sets it to its
    /// [`SessionSettings`] once they exist, and it must do so before the first
    /// `/permissions`. Until then the command reads settings generation `0`, so
    /// inspecting or clearing grants resets any grants kept for a later
    /// generation. A host that already has another `/permissions` command keeps
    /// it; this plugin logs the duplicate and registers nothing.
    #[must_use]
    pub fn with_commands(
        mut self,
        commands: Arc<CommandRegistry>,
        settings: Arc<OnceLock<SessionSettings>>,
    ) -> Self {
        self.commands = Some(commands);
        self.settings = Some(settings);
        self
    }
}

impl Plugin for PermissionRulesPlugin {
    const ID: &'static str = "polaris_code::permission_rules";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolPolicyPlugin>()]
    }

    fn default_dependencies(&self) -> DefaultDependencies {
        DefaultDependencies::new().add::<ToolPolicyPlugin>()
    }

    fn build(&self, server: &mut Server) {
        let mut rules = self
            .root
            .as_ref()
            .map_or_else(PermissionRules::default, |root| {
                PermissionRules::discover(root)
            });
        if let Some(trust) = &self.trust {
            rules = rules.with_project_trust(trust.clone());
        }
        let grants = SessionGrants::default();
        let shell = ShellApproval::new(
            server
                .get_global::<ShellExecutor>()
                .map(|value| value.clone()),
        );
        if let Some(commands) = &self.commands {
            let rules = rules.clone();
            let grants = grants.clone();
            let settings = self.settings.clone();
            if commands
                .register_sync(
                    CommandSpec::new("permissions", "List permission rules and session grants")
                        .with_usage("[clear-session|trust-project]")
                        .with_choices(["clear-session", "trust-project"]),
                    move |_, args| {
                        let generation = settings
                            .as_ref()
                            .and_then(|cell| cell.get())
                            .map_or(0, SessionSettings::approval_generation);
                        match args.trim() {
                            "" => CommandOutcome::Reply(format!(
                                "{}\n{}",
                                rules.describe(),
                                grants.describe(generation)
                            )),
                            "clear-session" => {
                                grants.clear(generation);
                                CommandOutcome::Reply("Session command grants cleared.".into())
                            }
                            "trust-project" => {
                                rules.trust_project();
                                CommandOutcome::Reply(format!(
                                    "Project allow rules enabled for this process.\n{}\n{}",
                                    remembered(&rules),
                                    rules.describe()
                                ))
                            }
                            _ => CommandOutcome::Error(
                                "Usage: /permissions [clear-session|trust-project]".into(),
                            ),
                        }
                    },
                )
                .is_err()
            {
                tracing::warn!("Could not register /permissions: command already exists");
            }
        }
        server.insert_global(self.tools.clone());
        server.insert_global(rules);
        server.insert_global(grants);
        server.insert_global(shell);
    }

    async fn ready(&self, server: &mut Server) {
        if let (Some(shell), Some(executor)) = (
            server.get_global::<ShellApproval>(),
            server.get_global::<ShellExecutor>(),
        ) {
            shell.bind(ShellExecutor::clone(&executor));
        }
    }
}

/// Save the trust decision and say what it now covers.
fn remembered(rules: &PermissionRules) -> String {
    match rules.remember_project_trust() {
        Ok(saved) if saved.is_empty() => {
            "This project has no settings or rule file to remember.".into()
        }
        Ok(saved) => format!(
            "Remembered for later sessions: {}. Restart to apply trusted settings.",
            saved
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Err(error) => {
            format!(
                "Could not remember this for later sessions ({error}); it applies to this one only."
            )
        }
    }
}

/// Disabled defaults used by hosts that do not install permission files.
pub(crate) fn default_dependencies() -> DefaultDependencies {
    DefaultDependencies::new()
        .add::<ToolPolicyPlugin>()
        .add::<PermissionRulesPlugin>()
}

/// Exact proposed call supplied to deterministic authorization deciders.
#[derive(Debug)]
pub struct PermissionCall<'a> {
    /// Registry tool name.
    pub tool: &'a str,
    /// Full unchanged model arguments.
    pub args: &'a serde_json::Value,
    /// Permission declared by the registered tool.
    pub registry_default: polaris::tools::ToolPermission,
    /// Canonical workspace root.
    pub root: &'a Path,
    /// Current settings approval lifetime.
    pub generation: u64,
    /// Operator's immutable ceiling, when supplied.
    pub ceiling: Option<&'a super::PermissionCeiling>,
}

/// Whether the automatic reviewer may settle a call left at
/// [`Confirm`](polaris::tools::ToolPermission::Confirm).
///
/// Dispatch checks this again against the decision it resolves at that moment,
/// so a review recorded under a different decision never authorizes a call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReviewEligibility {
    /// The reviewer may allow, deny or defer the call.
    #[default]
    Eligible,
    /// Only a person may allow the call. The reviewer is never asked, and its
    /// verdict is ignored if one exists. Set for an explicit `ask` rule, a
    /// project `allow` rule that is not yet trusted, and a tool whose
    /// capability requires a person for every attended call.
    HumanOnly,
}

/// Explained deterministic verdict before a reviewer or human is consulted.
#[derive(Debug, Clone)]
pub struct AuthorizationDecision {
    /// Whether this call runs, asks or is refused.
    pub permission: polaris::tools::ToolPermission,
    /// Winning rule, grant or mode; suitable for the prompt and trajectory.
    pub source: String,
    /// Whether the prompt may offer session scopes under this decision.
    pub remember: bool,
    /// Whether a matching human session grant already authorized this call.
    pub session_grant: bool,
    /// Whether the automatic reviewer may settle this call when it asks.
    pub review: ReviewEligibility,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{PermissionMode, PolicySnapshot};
    use crate::config::ModelCatalog;
    use polaris::plugins::{CONFIRMED, IOError, IOMessage, IOProvider, UserIO};
    use polaris::shell::ShellPlugin;
    use polaris::tools::ToolsPlugin;
    use polaris_ontrace::oxigraph::OxigraphStore;
    use polaris_tui::commands::CommandInvocation;
    use serde_json::json;

    #[tokio::test]
    async fn permission_plugin_loads_the_policy_seam_and_exposes_local_inspection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join("polaris.permissions.toml"),
            "[[rules]]\ntool = 'read_file'\ndecision = 'deny'\npath = '.env'\n",
        )
        .unwrap();
        let commands = Arc::new(CommandRegistry::with_builtins());
        let settings = Arc::new(OnceLock::new());
        settings
            .set(SessionSettings::new(
                "fake/model",
                PermissionMode::Confirm,
                &ModelCatalog::pinned("fake/model"),
            ))
            .unwrap();
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::with_working_dir(dir.path()))
            .add_plugins(
                PermissionRulesPlugin::new(dir.path()).with_commands(commands.clone(), settings),
            );
        server.finish().await.unwrap();
        let rules = server.get_global::<PermissionRules>().unwrap();
        let snapshot =
            PolicySnapshot::load(&OxigraphStore::in_memory().unwrap(), None, &rules).await;
        assert!(snapshot.permission.rules.is_some());
        assert!(server.get_global::<SessionGrants>().is_some());
        assert!(server.get_global::<RunToolGrants>().is_some());
        let outcome = commands
            .dispatch(&CommandInvocation::parse("/permissions").unwrap())
            .await;
        let CommandOutcome::Reply(text) = outcome else {
            panic!("inspection reply")
        };
        assert!(text.contains("polaris.permissions.toml"), "{text}");
        assert!(text.contains("read_file"));
        assert!(text.contains("Session command grants: 0"));
    }

    /// Build a session in `dir` and return its command registry and rules.
    async fn session(dir: &Path, trust: ProjectTrust) -> (Arc<CommandRegistry>, PermissionRules) {
        let commands = Arc::new(CommandRegistry::with_builtins());
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::with_working_dir(dir))
            .add_plugins(
                PermissionRulesPlugin::new(dir)
                    .with_project_trust(trust)
                    .with_commands(commands.clone(), Arc::new(OnceLock::new())),
            );
        server.finish().await.unwrap();
        let rules = PermissionRules::clone(&server.get_global::<PermissionRules>().unwrap());
        (commands, rules)
    }

    fn cargo_test_is_allowed(rules: &PermissionRules) -> bool {
        rules
            .resolve(
                "run_command",
                &serde_json::json!({"command":"cargo test --lib"}),
            )
            .is_some_and(|(decision, _)| decision == RuleDecision::Allow)
    }

    #[tokio::test]
    async fn trust_project_is_remembered_for_the_next_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join("polaris.permissions.toml"),
            "[[rules]]\ntool = 'run_command'\ndecision = 'allow'\ncommand_prefix = 'cargo test'\n",
        )
        .unwrap();
        let store = tempfile::tempdir().unwrap();
        let trust = || {
            ProjectTrust::new(
                Some(store.path().join(crate::project_trust::STORE_FILE)),
                dir.path(),
            )
        };
        let (commands, rules) = session(dir.path(), trust()).await;
        assert!(!cargo_test_is_allowed(&rules));
        let CommandOutcome::Reply(text) = commands
            .dispatch(&CommandInvocation::parse("/permissions trust-project").unwrap())
            .await
        else {
            panic!("trust reply")
        };
        assert!(
            text.contains("Remembered for later sessions: permission-rules"),
            "{text}"
        );
        assert!(cargo_test_is_allowed(&rules));
        let (_, next) = session(dir.path(), trust()).await;
        assert!(cargo_test_is_allowed(&next));
    }

    #[tokio::test]
    async fn trust_project_reports_when_it_cannot_be_remembered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(
            dir.path().join("polaris.permissions.toml"),
            "[[rules]]\ntool = 'run_command'\ndecision = 'allow'\ncommand_prefix = 'cargo test'\n",
        )
        .unwrap();
        let (commands, rules) = session(dir.path(), ProjectTrust::new(None, dir.path())).await;
        let CommandOutcome::Reply(text) = commands
            .dispatch(&CommandInvocation::parse("/permissions trust-project").unwrap())
            .await
        else {
            panic!("trust reply")
        };
        assert!(text.contains("Could not remember"), "{text}");
        assert!(text.contains("this one only"), "{text}");
        // The session still gets what it asked for.
        assert!(cargo_test_is_allowed(&rules));
    }

    /// Answers every prompt with a confirmation that picks one offered scope.
    struct Choose(&'static str);

    impl IOProvider for Choose {
        async fn send(&self, _: IOMessage) -> Result<(), IOError> {
            Ok(())
        }
        async fn receive(&self) -> Result<IOMessage, IOError> {
            Ok(IOMessage::user_text("choose")
                .with_metadata(CONFIRMED, "true")
                .with_metadata("approval_choice", self.0))
        }
    }

    /// A built host whose repository holds one project `allow` rule, with
    /// `/permissions` registered against live settings.
    struct Host {
        dir: tempfile::TempDir,
        server: Server,
        commands: Arc<CommandRegistry>,
        settings: SessionSettings,
    }

    impl Host {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join(".git")).unwrap();
            std::fs::write(
                dir.path().join("polaris.permissions.toml"),
                "[[rules]]\ntool = 'run_command'\ndecision = 'allow'\ncommand_prefix = 'cargo test'\n",
            )
            .unwrap();
            let settings = SessionSettings::new(
                "fake/model",
                PermissionMode::Confirm,
                &ModelCatalog::pinned("fake/model"),
            );
            let cell = Arc::new(OnceLock::new());
            cell.set(settings.clone()).unwrap();
            let commands = Arc::new(CommandRegistry::with_builtins());
            let mut server = Server::new();
            server.add_plugins(ToolsPlugin).add_plugins(
                PermissionRulesPlugin::new(dir.path()).with_commands(commands.clone(), cell),
            );
            server.finish().await.unwrap();
            Self {
                dir,
                server,
                commands,
                settings,
            }
        }

        async fn run(&self, line: &str) -> CommandOutcome {
            self.commands
                .dispatch(&CommandInvocation::parse(line).unwrap())
                .await
        }
    }

    #[tokio::test]
    async fn clear_session_revokes_remembered_commands() {
        let host = Host::new().await;
        let grants = host.server.get_global::<SessionGrants>().unwrap().clone();
        let generation = host.settings.approval_generation();
        let args = json!({"command":"cargo test"});
        let request = ApprovalRequest::new("run_command", &args, host.dir.path(), generation, true);
        let scope = request
            .ask(
                &UserIO::new(Arc::new(Choose("exact"))),
                "Run it?".into(),
                &grants,
            )
            .await
            .unwrap();
        assert_eq!(scope, ApprovalScope::Exact);
        assert!(grants.allows(generation, "run_command", &args, host.dir.path()));
        let CommandOutcome::Reply(listing) = host.run("/permissions").await else {
            panic!("inspection replies");
        };
        assert!(listing.contains("Session command grants: 1"), "{listing}");
        let CommandOutcome::Reply(reply) = host.run("/permissions clear-session").await else {
            panic!("clearing replies");
        };
        assert_eq!(reply, "Session command grants cleared.");
        assert!(!grants.allows(generation, "run_command", &args, host.dir.path()));
        assert!(
            grants
                .describe(generation)
                .contains("Session command grants: 0")
        );
    }

    #[tokio::test]
    async fn trust_project_enables_project_allow_rules_and_changes_the_revision() {
        let host = Host::new().await;
        let rules = host.server.get_global::<PermissionRules>().unwrap().clone();
        let args = json!({"command":"cargo test"});
        let revision = rules.revision();
        assert_eq!(
            rules.resolve("run_command", &args).unwrap().0,
            RuleDecision::Ask
        );
        let CommandOutcome::Reply(listing) = host.run("/permissions").await else {
            panic!("inspection replies");
        };
        assert!(
            listing.contains("[asks until /permissions trust-project]"),
            "{listing}"
        );
        let CommandOutcome::Reply(reply) = host.run("/permissions trust-project").await else {
            panic!("trusting replies");
        };
        assert!(
            reply.starts_with("Project allow rules enabled for this process."),
            "{reply}"
        );
        assert!(!reply.contains("asks until"), "{reply}");
        assert_eq!(rules.revision(), revision + 1);
        assert_eq!(
            rules.resolve("run_command", &args).unwrap().0,
            RuleDecision::Allow
        );
    }

    #[tokio::test]
    async fn an_unknown_argument_changes_nothing_and_states_the_usage() {
        let host = Host::new().await;
        let rules = host.server.get_global::<PermissionRules>().unwrap().clone();
        let revision = rules.revision();
        for line in ["/permissions forever", "/permissions trust-project now"] {
            let CommandOutcome::Error(message) = host.run(line).await else {
                panic!("{line} is refused");
            };
            assert_eq!(message, "Usage: /permissions [clear-session|trust-project]");
        }
        assert_eq!(rules.revision(), revision);
    }

    #[tokio::test]
    async fn the_shell_executor_is_bound_whichever_plugin_is_registered_first() {
        let dir = tempfile::tempdir().unwrap();
        let args = json!({"command":"echo hi"});
        let mut shell_first = Server::new();
        shell_first
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::with_working_dir(dir.path()))
            .add_plugins(ToolPolicyPlugin)
            .add_plugins(PermissionRulesPlugin::default());
        let mut rules_first = Server::new();
        rules_first
            .add_plugins(PermissionRulesPlugin::default())
            .add_plugins(ToolPolicyPlugin)
            .add_plugins(ShellPlugin::with_working_dir(dir.path()))
            .add_plugins(ToolsPlugin);
        for (order, mut server) in [("shell first", shell_first), ("rules first", rules_first)] {
            server.finish().await.unwrap();
            let shell = server.get_global::<ShellApproval>().unwrap();
            assert!(shell.permission(&args).is_some(), "{order}");
        }
    }

    #[tokio::test]
    async fn an_existing_permissions_command_is_kept_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let commands = Arc::new(CommandRegistry::with_builtins());
        commands
            .register_sync(CommandSpec::new("permissions", "Host's own"), |_, _| {
                CommandOutcome::Reply("host".into())
            })
            .unwrap();
        let mut server = Server::new();
        server.add_plugins(ToolsPlugin).add_plugins(
            PermissionRulesPlugin::new(dir.path())
                .with_commands(commands.clone(), Arc::new(OnceLock::new())),
        );
        server.finish().await.unwrap();
        let CommandOutcome::Reply(text) = commands
            .dispatch(&CommandInvocation::parse("/permissions").unwrap())
            .await
        else {
            panic!("the host's command replies");
        };
        assert_eq!(text, "host");
        assert!(server.get_global::<PermissionRules>().is_some());
    }
}
