use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use polaris::plugins::{
    CONFIRMED, CONFIRMED_FALSE, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOError,
    IOMessage, UserIO,
};
use polaris::shell::{ShellExecutor, ShellPermission};
use polaris::system::resource::GlobalResource;
use polaris::tools::{ToolContext, ToolError, ToolPermission, ToolRegistry};
use polaris_tui::{
    APPROVAL_CHOICE, APPROVAL_CHOICES, APPROVAL_REQUEST_ID, ApprovalChoice,
    IO_TYPE_APPROVAL_RESOLVED,
};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::terminal::background::ToolRun;
use crate::tools::approval::{ApprovalReceipt, call_directory, call_paths};

use super::rules::{command_permission, simple_prefix};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// A user's refusal is distinct from failure of the approval channel.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApprovalError {
    /// The user denied the call or selected a scope that was not offered.
    Denied {
        /// Denial text suitable for returning to the requesting model.
        reason: String,
    },
    /// The prompt could not be sent or its response could not be received.
    Prompt(IOError),
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Denied { reason } => f.write_str(reason),
            Self::Prompt(error) => write!(f, "Confirmation failed: {error}"),
        }
    }
}
impl std::error::Error for ApprovalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Prompt(error) => Some(error),
            Self::Denied { .. } => None,
        }
    }
}

/// Lifetime and breadth deliberately selected by a person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScope {
    /// Authorize this exact invocation only.
    Once,
    /// Remember this literal command and directory for the session.
    Exact,
    /// Remember the displayed command prefix and directory for the session.
    Prefix,
}

#[derive(Debug, Clone)]
struct Grant {
    tool: String,
    command: String,
    directory: PathBuf,
    prefix: bool,
    patterns: Vec<String>,
}

#[derive(Debug, Default)]
struct GrantState {
    generation: u64,
    grants: Vec<Grant>,
}

/// Human-approved command scopes for the active settings generation.
///
/// Provided by [`super::PermissionRulesPlugin`] as a shared global handle so
/// `/permissions` and the agent inspect the same grants. Every read/write checks
/// the session generation; `/new`, `/resume` and explicit revocation invalidate
/// old grants. A grant never crosses tool names or canonical directories.
///
/// # Scope
///
/// Global handle because policy and `/permissions` must share revocation state;
/// stored grants last only for the active session generation.
///
/// # Provided By
///
/// [`super::PermissionRulesPlugin`] inserts the handle during `build`.
///
/// # Access
///
/// Use `Res<SessionGrants>`. Clones share a mutex; every method checks the supplied
/// generation and invalidates older grants. `clear` revokes remembered scopes.
///
/// # Alternatives
///
/// [`super::PermissionRules`] supplies file decisions; [`super::RunToolGrants`]
/// supplies unattended launch grants; [`ApprovalScope`] describes prompt choices.
///
/// # Example
///
/// ```
/// use polaris::system::{param::Res, system};
/// use polaris_code::agents::permission_rules::SessionGrants;
/// use polaris_code::terminal::SessionSettings;
/// #[system]
/// async fn inspect_grants(grants: Res<SessionGrants>, settings: Res<SessionSettings>) {
///     tracing::debug!(grants = %grants.describe(settings.approval_generation()));
/// }
/// ```
#[derive(Debug, Default, Clone)]
pub struct SessionGrants(Arc<Mutex<GrantState>>);

impl GlobalResource for SessionGrants {}

impl SessionGrants {
    fn state(&self, generation: u64) -> std::sync::MutexGuard<'_, GrantState> {
        let mut state = self.0.lock().unwrap_or_else(|err| err.into_inner());
        if state.generation != generation {
            *state = GrantState {
                generation,
                grants: Vec::new(),
            };
        }
        state
    }

    /// Revoke every remembered command in this session.
    pub fn clear(&self, generation: u64) {
        self.state(generation).grants.clear();
    }

    /// Show exact remembered rules, including tool and canonical directory.
    #[must_use]
    pub fn describe(&self, generation: u64) -> String {
        let state = self.state(generation);
        let mut lines = vec![format!("Session command grants: {}", state.grants.len())];
        lines.extend(state.grants.iter().map(|grant| {
            format!(
                "- {} {} {:?} in {} (until session reset)",
                grant.tool,
                if grant.prefix { "prefix" } else { "exact" },
                grant.command,
                grant.directory.display()
            )
        }));
        lines.join("\n")
    }

    /// Whether a remembered human scope admits this command.
    #[must_use]
    pub fn allows(&self, generation: u64, tool: &str, args: &Value, root: &Path) -> bool {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return false;
        };
        let Some(directory) = call_directory(args, root) else {
            return false;
        };
        self.state(generation).grants.iter().any(|grant| {
            grant.tool == tool
                && grant.directory == directory
                && if grant.prefix {
                    command_permission(command, &grant.patterns, &[]) == ShellPermission::Allow
                } else {
                    grant.command == command
                }
        })
    }

    fn remember(&self, request: &ApprovalRequest, scope: ApprovalScope) {
        let command = match scope {
            ApprovalScope::Once => return,
            ApprovalScope::Exact => request.command.clone(),
            ApprovalScope::Prefix => request.prefix.clone(),
        };
        if let (Some(command), Some(directory)) = (command, request.directory.clone()) {
            self.state(request.generation).grants.push(Grant {
                tool: request.tool.clone(),
                patterns: vec![command.clone(), format!("{command} *")],
                command,
                directory,
                prefix: scope == ApprovalScope::Prefix,
            });
        }
    }
}

/// Host-provided choices for one exact call; front ends render their labels.
///
/// The metadata extension uses `approval_choices` (JSON objects with `id`,
/// `label`, `keys`, `confirmed`) and `approval_choice` on the response. The
/// existing `io_type=confirm` and `confirmed` protocol remains authoritative.
/// Unknown choices, ordinary prompts and missing confirmation metadata deny.
#[derive(Debug)]
pub struct ApprovalRequest {
    tool: String,
    args: Value,
    root: PathBuf,
    command: Option<String>,
    directory: Option<PathBuf>,
    prefix: Option<String>,
    generation: u64,
    remember: bool,
    id: String,
}

impl ApprovalRequest {
    /// Build choices for the exact tool arguments and canonical directory.
    /// `remember` must be false when a ceiling or an ask rule prevents grants.
    #[must_use]
    pub fn new(tool: &str, args: &Value, root: &Path, generation: u64, remember: bool) -> Self {
        let command = matches!(tool, "run_command" | "run_in_background")
            .then(|| {
                args.get("command")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .flatten();
        let prefix = command
            .as_deref()
            .filter(|text| simple_prefix(text))
            .map(|text| {
                text.split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            });
        let (root, directory) = call_paths(args, root).map_or_else(
            || (root.to_path_buf(), None),
            |(root, directory)| (root, Some(directory)),
        );
        Self {
            tool: tool.into(),
            args: args.clone(),
            root,
            command,
            directory,
            prefix,
            generation,
            remember,
            id: NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).to_string(),
        }
    }

    fn choices(&self) -> Vec<ApprovalChoice> {
        let mut choices = vec![ApprovalChoice::new(
            "once",
            "allow once",
            &["y", "yes", "once"],
            true,
        )];
        if self.remember
            && let (Some(command), Some(directory)) = (&self.command, &self.directory)
        {
            choices.push(ApprovalChoice::new(
                "exact",
                format!(
                    "allow exact command {command:?} in {} for this session",
                    directory.display()
                ),
                &["s", "exact"],
                true,
            ));
            if let Some(prefix) = &self.prefix {
                choices.push(ApprovalChoice::new("prefix", format!("allow prefix {prefix:?} (whole words, every subcommand) in {} for this session", directory.display()), &["p", "prefix"], true));
            }
        }
        choices.push(ApprovalChoice::new(
            "deny",
            "deny (optional: deny: reason)",
            &["n", "no", "deny"],
            false,
        ));
        choices
    }

    /// Resolve an interrupted prompt as denied before the next receive.
    ///
    /// # Errors
    ///
    /// Returns the provider error if the resolution cannot be delivered.
    pub async fn cancel(&self, io: &UserIO) -> Result<(), IOError> {
        io.send(
            IOMessage::system_text("Approval interrupted by user")
                .with_metadata(IO_TYPE, IO_TYPE_APPROVAL_RESOLVED)
                .with_metadata(CONFIRMED, CONFIRMED_FALSE)
                .with_metadata(APPROVAL_REQUEST_ID, &self.id),
        )
        .await
    }

    /// Ask through the standard confirmation channel and remember only an
    /// explicitly offered scope. A denial reason is returned to the model.
    ///
    /// If the future is cancelled, call [`Self::cancel`] before receiving again.
    ///
    /// # Errors
    ///
    /// Returns [`ApprovalError::Denied`] for refusal or an unavailable scope,
    /// and [`ApprovalError::Prompt`] when sending or receiving the prompt fails.
    pub async fn ask(
        &self,
        io: &UserIO,
        prompt: String,
        grants: &SessionGrants,
    ) -> Result<ApprovalScope, ApprovalError> {
        let choices = self.choices();
        let prompt = if self.command.is_some() {
            format!(
                "{prompt}\nTool: {}\nDirectory: {}\nArguments: {}",
                self.tool,
                self.directory.as_ref().map_or_else(
                    || "<invalid directory>".into(),
                    |dir| dir.display().to_string()
                ),
                self.args
            )
        } else {
            prompt
        };
        let labels = choices
            .iter()
            .map(|choice| {
                format!(
                    "{}: {}",
                    choice.keys.first().map_or("", String::as_str),
                    choice.label
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        io.send(
            IOMessage::system_text(format!("{prompt}\n\n{labels}"))
                .with_metadata(IO_TYPE, IO_TYPE_CONFIRM)
                .with_metadata(APPROVAL_CHOICES, ApprovalChoice::encode(&choices))
                .with_metadata(APPROVAL_REQUEST_ID, &self.id),
        )
        .await
        .map_err(ApprovalError::Prompt)?;
        let response = io.receive().await.map_err(ApprovalError::Prompt)?;
        if response
            .metadata
            .get(APPROVAL_REQUEST_ID)
            .is_some_and(|id| id != &self.id)
        {
            return Err(ApprovalError::Denied {
                reason: "Confirmation does not match this approval request".into(),
            });
        }
        if !response
            .metadata
            .get(CONFIRMED)
            .is_some_and(|value| value == CONFIRMED_TRUE)
        {
            let reason = match &response.content {
                IOContent::Text(text) => text
                    .trim()
                    .split_once(':')
                    .filter(|(key, _)| {
                        key.eq_ignore_ascii_case("deny") || key.eq_ignore_ascii_case("no")
                    })
                    .map_or("", |(_, reason)| reason.trim()),
                _ => "",
            };
            return Err(ApprovalError::Denied {
                reason: if reason.is_empty() {
                    format!("User denied execution of tool '{}'", self.tool)
                } else {
                    format!("User denied execution of tool '{}': {reason}", self.tool)
                },
            });
        }
        let id = response
            .metadata
            .get(APPROVAL_CHOICE)
            .map_or("once", String::as_str);
        if !choices
            .iter()
            .any(|choice| choice.id == id && choice.confirmed)
        {
            return Err(ApprovalError::Denied {
                reason: "Confirmation selected an unavailable approval scope".into(),
            });
        }
        let scope = match id {
            "exact" => ApprovalScope::Exact,
            "prefix" => ApprovalScope::Prefix,
            _ => ApprovalScope::Once,
        };
        grants.remember(self, scope);
        Ok(scope)
    }
}

/// Confirmed foreground dispatch over the shell capability's existing executor.
///
/// Provided by [`super::PermissionRulesPlugin`]. The disabled default delegates
/// to the registry. Receipts are issued only after argument-aware authorization;
/// deny patterns and working-directory checks still run in the registered tool.
/// Install [`crate::tools::approval::ApprovalShellPlugin`] alongside the shell
/// plugin for receipt-aware foreground execution, or supply a replacement tool that consumes the receipt.
///
/// # Scope
///
/// Global because all sessions compose the same shell executor; receipts are
/// bound to the issuing session's generation and the current rule revision.
///
/// # Provided By
///
/// [`super::PermissionRulesPlugin`], which binds the optional executor in `ready`.
///
/// # Access
///
/// Use `Res<ShellApproval>`. Clones share a write-once executor binding. `resolve`
/// reads policy and shell restrictions. `spawn` validates receipt context and
/// invokes the registered tool; the tool alone consumes the receipt.
///
/// # Alternatives
///
/// [`ToolRegistry::execute`] provides ordinary dispatch;
/// [`crate::agents::PermissionPolicy::resolve_call`] supplies deterministic policy
/// before shell restrictions. An embedding host can issue [`ApprovalReceipt`]
/// directly with its own policy validator.
///
/// # Example
///
/// ```
/// use polaris::system::{param::Res, system};
/// use polaris::tools::{ToolContext, ToolPermission, ToolRegistry};
/// use polaris_code::agents::PermissionMode;
/// use polaris_code::agents::permission_rules::{PermissionCall, SessionGrants, ShellApproval};
/// use polaris_code::terminal::SessionSettings;
/// #[system]
/// async fn dispatch(
///     shell: Res<ShellApproval>, tools: Res<ToolRegistry>,
///     grants: Res<SessionGrants>, settings: Res<SessionSettings>,
/// ) {
///     let args = serde_json::json!({"command":"cargo test"});
///     let call = PermissionCall {
///         tool: "run_command", args: &args,
///         registry_default: tools.permission("run_command").unwrap_or(ToolPermission::Deny),
///         root: std::path::Path::new("."),
///         generation: settings.approval_generation(), ceiling: None,
///     };
///     let policy = PermissionMode::Confirm.permission_policy();
///     if shell.resolve(&policy, &call, &grants).permission == ToolPermission::Allow {
///         let result = shell.spawn(&tools, "run_command", args, ToolContext::new()).await;
///         tracing::debug!(?result);
///     }
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct ShellApproval(Arc<OnceLock<ShellExecutor>>);

impl GlobalResource for ShellApproval {}

impl ShellApproval {
    pub(super) fn new(executor: Option<ShellExecutor>) -> Self {
        let access = Self::default();
        if let Some(executor) = executor {
            access.bind(executor);
        }
        access
    }

    pub(super) fn bind(&self, executor: ShellExecutor) {
        let _ = self.0.set(executor);
    }

    /// Shell's hard deny/opaque-syntax verdict, when shell capability is installed.
    #[must_use]
    pub fn permission(&self, args: &Value) -> Option<ShellPermission> {
        self.0.get().and_then(|executor| {
            args.get("command")
                .and_then(Value::as_str)
                .map(|command| executor.check_permission(command))
        })
    }

    /// Apply shell hard denies and opaque-syntax checks after deterministic
    /// rules. Only a deliberately remembered exact human approval may cover
    /// opaque shell text without a new prompt.
    #[must_use]
    pub fn resolve(
        &self,
        policy: &crate::agents::PermissionPolicy,
        call: &super::PermissionCall<'_>,
        grants: &SessionGrants,
    ) -> super::AuthorizationDecision {
        let mut decision = policy.resolve_call(call, grants);
        if matches!(call.tool, "run_command" | "run_in_background") {
            if call_directory(call.args, call.root).is_none() {
                decision.permission = ToolPermission::Deny;
                decision.source = "shell directory is invalid or outside the workspace".into();
                decision.remember = false;
                return decision;
            }
            match self.permission(call.args) {
                Some(ShellPermission::Deny) => {
                    decision.permission = ToolPermission::Deny;
                    decision.source = "this command is on the shell's deny list".into();
                    decision.remember = false;
                }
                Some(ShellPermission::Confirm)
                    if decision.permission == ToolPermission::Allow && !decision.session_grant =>
                {
                    decision.permission = if policy.is_unattended() {
                        ToolPermission::Deny
                    } else {
                        ToolPermission::Confirm
                    };
                    decision.source =
                        "shell syntax or command policy requires exact approval".into();
                }
                _ => {}
            }
        }
        decision
    }

    #[cfg(test)]
    fn receipt(
        &self,
        tool: &str,
        args: &Value,
        root: &Path,
        rules: &super::PermissionRules,
        settings: &crate::terminal::SessionSettings,
    ) -> Option<ApprovalReceipt> {
        self.receipt_for(
            &ApprovalRequest::new(tool, args, root, settings.approval_generation(), false),
            rules,
            settings,
        )
    }

    pub(crate) fn receipt_for(
        &self,
        request: &ApprovalRequest,
        rules: &super::PermissionRules,
        settings: &crate::terminal::SessionSettings,
    ) -> Option<ApprovalReceipt> {
        if self.0.get().is_none()
            || !matches!(request.tool.as_str(), "run_command" | "run_in_background")
        {
            return None;
        }
        let revision = rules.revision();
        let generation = request.generation;
        let rules = rules.clone();
        let settings = settings.clone();
        Some(ApprovalReceipt::issue(
            &request.tool,
            request.args.clone(),
            &request.root,
            request.directory.as_deref()?,
            move || rules.revision() == revision && settings.approval_generation() == generation,
        ))
    }

    /// Spawn a tool with the existing background-wait lifecycle. The confirmed
    /// path consumes host context, never a model-visible boolean parameter.
    #[must_use]
    pub fn spawn(
        &self,
        registry: &ToolRegistry,
        name: &str,
        args: Value,
        context: ToolContext,
    ) -> ToolRun {
        let tool = registry.to_arc(name);
        let name = name.to_string();
        ToolRun::spawn_future(async move {
            let tool = tool.ok_or_else(|| ToolError::unknown_tool(&name))?;
            if let Some(receipt) = context.get::<ApprovalReceipt>()
                && !receipt.matches(&name, &args)
            {
                return Err(ToolError::permission_denied(
                    "Approval does not match this invocation",
                ));
            }
            tool.execute(args, &context).await
        })
    }
}

pub(crate) fn ceiling_permission(
    permission: ToolPermission,
    ceiling: Option<ToolPermission>,
) -> ToolPermission {
    match (permission, ceiling) {
        (_, Some(ToolPermission::Deny)) | (ToolPermission::Deny, _) => ToolPermission::Deny,
        (ToolPermission::Allow, Some(ToolPermission::Confirm)) => ToolPermission::Confirm,
        (value, _) => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{PermissionCeiling, PermissionMode};
    use crate::terminal::{BackgroundTasks, SessionSettings};
    use crate::tools::approval::ApprovalShellTool;
    use polaris::plugins::{IOError, IOProvider};
    use polaris::shell::ShellConfig;
    use std::time::Duration;

    struct Answer {
        reply: IOMessage,
        prompts: Mutex<Vec<IOMessage>>,
    }
    impl IOProvider for Answer {
        async fn send(&self, message: IOMessage) -> Result<(), IOError> {
            self.prompts.lock().unwrap().push(message);
            Ok(())
        }
        async fn receive(&self) -> Result<IOMessage, IOError> {
            Ok(self.reply.clone())
        }
    }
    fn io(choice: &str, confirmed: bool) -> (UserIO, Arc<Answer>) {
        let provider = Arc::new(Answer {
            reply: IOMessage::user_text(choice)
                .with_metadata(CONFIRMED, if confirmed { "true" } else { "false" })
                .with_metadata(APPROVAL_CHOICE, choice),
            prompts: Mutex::new(Vec::new()),
        });
        (UserIO::new(provider.clone()), provider)
    }
    fn settings() -> SessionSettings {
        SessionSettings::compatibility_default("fake/model")
    }
    fn shell(root: &Path) -> (ShellApproval, ShellExecutor, ToolRegistry) {
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_working_dir(root)
                .with_allowed_dirs(vec![root.into()])
                .with_allowed_commands(vec!["*".into()])
                .with_denied_commands(vec!["echo forbidden".into(), "touch denied-*".into()]),
        );
        let mut registry = ToolRegistry::new();
        registry.register(ApprovalShellTool::new(executor.clone()));
        (
            ShellApproval::new(Some(executor.clone())),
            executor,
            registry,
        )
    }

    #[tokio::test]
    async fn opaque_approved_foreground_runs_exactly_once_and_receipts_cannot_replay() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, executor, registry) = shell(dir.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let args = json!({"command": "printf x >> marker; echo \"$(pwd)\"", "timeout_secs":5});
        let missing = shell
            .spawn(&registry, "run_command", args.clone(), ToolContext::new())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(missing["status"], "confirmation_required");
        assert!(!dir.path().join("marker").exists());
        let request = ApprovalRequest::new(
            "run_command",
            &args,
            dir.path(),
            settings.approval_generation(),
            true,
        );
        let (io, _) = io("once", true);
        assert_eq!(
            request
                .ask(&io, "Run command?".into(), &SessionGrants::default())
                .await
                .unwrap(),
            ApprovalScope::Once
        );
        let receipt = shell
            .receipt("run_command", &args, dir.path(), &rules, &settings)
            .unwrap();
        let context = ToolContext::new().with(receipt);
        let result = shell
            .spawn(&registry, "run_command", args.clone(), context.clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result["status"], "executed");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(std::fs::read(dir.path().join("marker")).unwrap(), b"x");
        assert!(
            shell
                .spawn(&registry, "run_command", args, context)
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join("marker")).unwrap(), b"x");
        executor.stop_all().await;
    }

    #[tokio::test]
    async fn changed_arguments_policy_session_and_denied_commands_cannot_use_a_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, _, registry) = shell(dir.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let args = json!({"command":"echo \"$(pwd)\""});
        let receipt = shell
            .receipt("run_command", &args, dir.path(), &rules, &settings)
            .unwrap();
        let changed = json!({"command":"echo \"$(pwd)\"", "timeout_secs":1});
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    changed,
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
        let receipt = shell
            .receipt("run_command", &args, dir.path(), &rules, &settings)
            .unwrap();
        settings.reset("fake/model");
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    args.clone(),
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
        let receipt = shell
            .receipt("run_command", &args, dir.path(), &rules, &settings)
            .unwrap();
        rules.trust_project();
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    args,
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
        let denied = json!({"command":"echo forbidden"});
        let receipt = shell
            .receipt("run_command", &denied, dir.path(), &rules, &settings)
            .unwrap();
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    denied,
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_deny_listed_command_is_refused_up_front_and_again_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, executor, registry) = shell(dir.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let args = json!({"command":"touch denied-marker"});

        // Before any prompt: the verdict is Deny, with a reason that names the
        // command rather than the whole tool.
        let policy = PermissionMode::Confirm.permission_policy();
        let call = super::super::PermissionCall {
            tool: "run_command",
            args: &args,
            registry_default: ToolPermission::Confirm,
            root: dir.path(),
            generation: settings.approval_generation(),
            ceiling: None,
        };
        let decision = shell.resolve(&policy, &call, &SessionGrants::default());
        assert_eq!(decision.permission, ToolPermission::Deny);
        assert!(
            !decision.remember,
            "a denied command offers no session grant"
        );
        assert!(decision.source.contains("deny list"), "{}", decision.source);

        // Even with a receipt from a completed approval, the confirmed path
        // still applies the executor's deny list and starts nothing.
        let receipt = shell
            .receipt("run_command", &args, dir.path(), &rules, &settings)
            .unwrap();
        let error = shell
            .spawn(
                &registry,
                "run_command",
                args,
                ToolContext::new().with(receipt),
            )
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            error.to_string().contains("denied by policy"),
            "the refusal comes from the shell's deny list: {error}"
        );
        assert!(!dir.path().join("denied-marker").exists());
        executor.stop_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn changing_a_directory_alias_while_approval_is_pending_invalidates_the_receipt() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("before")).unwrap();
        std::fs::create_dir(dir.path().join("after")).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path().join("before"), &alias).unwrap();
        let (shell, _, registry) = shell(dir.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let args = json!({"command":"echo \"$(pwd)\"", "working_dir":"alias"});
        let request = ApprovalRequest::new(
            "run_command",
            &args,
            dir.path(),
            settings.approval_generation(),
            true,
        );
        let receipt = shell.receipt_for(&request, &rules, &settings).unwrap();
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(dir.path().join("after"), &alias).unwrap();
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    args,
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_directory_change_before_receipt_issuance_cannot_fall_back_to_unapproved_execution() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("before")).unwrap();
        std::fs::create_dir(directory.path().join("after")).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(directory.path().join("before"), &alias).unwrap();
        let (shell, executor, registry) = shell(directory.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let args = json!({"command":"touch marker", "working_dir":"alias"});
        let request = ApprovalRequest::new(
            "run_command",
            &args,
            directory.path(),
            settings.approval_generation(),
            false,
        );
        std::fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(directory.path().join("after"), &alias).unwrap();
        let receipt = shell
            .receipt_for(&request, &rules, &settings)
            .expect("an invalidated approval must still gate dispatch");
        assert!(
            shell
                .spawn(
                    &registry,
                    "run_command",
                    args,
                    ToolContext::new().with(receipt)
                )
                .await
                .unwrap()
                .is_err()
        );
        assert!(!directory.path().join("before/marker").exists());
        assert!(!directory.path().join("after/marker").exists());
        executor.stop_all().await;
    }

    #[tokio::test]
    async fn confirmed_background_uses_the_same_single_use_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, executor, mut registry) = shell(dir.path());
        let settings = settings();
        let rules = super::super::PermissionRules::default();
        let tasks = BackgroundTasks::detached(Duration::from_secs(5));
        registry.register_toolset(crate::tools::TaskTools::new(
            executor.clone(),
            tasks.clone(),
        ));
        let args = json!({"command":"printf x >> marker; echo \"$(pwd)\"", "timeout_secs":5});
        let receipt = shell
            .receipt("run_in_background", &args, dir.path(), &rules, &settings)
            .unwrap();
        let invocation = tasks.enter("background-test", "run_in_background", None);
        let sink = invocation.sink(
            &polaris::sessions::SessionId::from_string("test"),
            0,
            Duration::from_secs(5),
        );
        let context = ToolContext::new()
            .with(receipt)
            .with(polaris::shell::ShellSinkHandle::new(sink));
        let mut changed = args.clone();
        changed
            .as_object_mut()
            .unwrap()
            .insert("unapproved".into(), Value::Bool(true));
        assert!(
            shell
                .spawn(&registry, "run_in_background", changed, context.clone())
                .await
                .unwrap()
                .is_err()
        );
        assert!(!dir.path().join("marker").exists());
        let result = shell
            .spawn(
                &registry,
                "run_in_background",
                args.clone(),
                context.clone(),
            )
            .await
            .unwrap()
            .unwrap();
        assert!(result.as_str().unwrap().contains("Started task"));
        assert!(
            shell
                .spawn(&registry, "run_in_background", args, context)
                .await
                .unwrap()
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while tasks.finished().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("marker")).unwrap(), b"x");
        executor.stop_all().await;
    }

    #[tokio::test]
    async fn session_scopes_are_exact_or_decomposed_prefix_and_reset_with_settings() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings();
        let grants = SessionGrants::default();
        let args = json!({"command":"cargo test --lib"});
        let request = ApprovalRequest::new(
            "run_command",
            &args,
            dir.path(),
            settings.approval_generation(),
            true,
        );
        let (io, prompts) = io("prefix", true);
        assert_eq!(
            request.ask(&io, "Run?".into(), &grants).await.unwrap(),
            ApprovalScope::Prefix
        );
        let prompt = &prompts.prompts.lock().unwrap()[0];
        let IOContent::Text(text) = &prompt.content else {
            panic!("text prompt")
        };
        assert!(text.contains("prefix \"cargo test\""));
        assert!(grants.allows(
            settings.approval_generation(),
            "run_command",
            &json!({"command":"cargo test --all"}),
            dir.path()
        ));
        for command in [
            "cargo testing",
            "cargo test; echo surprise",
            "cargo test $(pwd)",
        ] {
            assert!(!grants.allows(
                settings.approval_generation(),
                "run_command",
                &json!({"command":command}),
                dir.path()
            ));
        }
        assert!(!grants.allows(
            settings.approval_generation(),
            "run_in_background",
            &args,
            dir.path()
        ));
        settings.reset("fake/model");
        assert!(!grants.allows(
            settings.approval_generation(),
            "run_command",
            &args,
            dir.path()
        ));
    }

    #[tokio::test]
    async fn exact_session_scope_can_cover_opaque_text_but_ceiling_and_ask_still_win() {
        let dir = tempfile::tempdir().unwrap();
        let settings = settings();
        let grants = SessionGrants::default();
        let args = json!({"command":"echo \"$(pwd)\""});
        let request = ApprovalRequest::new(
            "run_command",
            &args,
            dir.path(),
            settings.approval_generation(),
            true,
        );
        let (io, _) = io("exact", true);
        request.ask(&io, "Run?".into(), &grants).await.unwrap();
        let (shell, _, _) = shell(dir.path());
        let mut policy = PermissionMode::Confirm.permission_policy();
        let ceiling = PermissionCeiling::new(PermissionMode::Confirm).unwrap();
        let mut call = super::super::PermissionCall {
            tool: "run_command",
            args: &args,
            registry_default: ToolPermission::Allow,
            root: dir.path(),
            generation: settings.approval_generation(),
            ceiling: None,
        };
        assert_eq!(
            shell.resolve(&policy, &call, &grants).permission,
            ToolPermission::Allow
        );
        let user = dir.path().join("permissions.toml");
        std::fs::write(&user, "[[rules]]\ntool='run_command'\ndecision='allow'\n").unwrap();
        policy.rules = Some(super::super::PermissionRules::from_paths(
            dir.path(),
            Some(&user),
            None,
        ));
        assert_eq!(
            shell.resolve(&policy, &call, &grants).permission,
            ToolPermission::Allow
        );
        call.ceiling = Some(&ceiling);
        let decision = shell.resolve(&policy, &call, &grants);
        assert_eq!(decision.permission, ToolPermission::Confirm);
        assert!(!decision.remember);
        call.ceiling = None;
        std::fs::write(&user, "[[rules]]\ntool='run_command'\ndecision='ask'\n").unwrap();
        policy.rules = Some(super::super::PermissionRules::from_paths(
            dir.path(),
            Some(&user),
            None,
        ));
        let decision = shell.resolve(&policy, &call, &grants);
        assert_eq!(decision.permission, ToolPermission::Confirm);
        assert!(!decision.remember);
    }

    #[tokio::test]
    async fn unknown_scope_missing_metadata_and_denial_reason_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let request = ApprovalRequest::new(
            "run_command",
            &json!({"command":"echo ok"}),
            dir.path(),
            1,
            false,
        );
        let grants = SessionGrants::default();
        let (exact, _) = io("exact", true);
        assert!(request.ask(&exact, "Run?".into(), &grants).await.is_err());
        let denied = UserIO::new(Arc::new(Answer {
            reply: IOMessage::user_text("deny: inspect it first"),
            prompts: Mutex::new(Vec::new()),
        }));
        assert!(
            request
                .ask(&denied, "Run?".into(), &grants)
                .await
                .unwrap_err()
                .to_string()
                .contains("inspect it first")
        );
        assert!(!grants.allows(1, "run_command", &json!({"command":"echo ok"}), dir.path()));
    }

    #[tokio::test]
    async fn approved_dispatch_honors_a_replacement_tool_refusal() {
        use polaris::models::llm::ToolDefinition;
        use polaris::tools::Tool;
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Refuse(Arc<AtomicBool>);
        impl Tool for Refuse {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new("run_command", "Refuse execution", json!({"type":"object"}))
            }
            fn execute<'a>(
                &'a self,
                _args: Value,
                _context: &'a ToolContext,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + 'a>,
            > {
                Box::pin(async move {
                    self.0.store(true, Ordering::SeqCst);
                    Err(ToolError::permission_denied("replacement refuses"))
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (shell, executor, _) = shell(dir.path());
        let called = Arc::new(AtomicBool::new(false));
        let mut registry = ToolRegistry::new();
        registry.register(Refuse(called.clone()));
        let args = json!({"command":"touch marker; echo \"$(pwd)\""});
        let receipt = shell
            .receipt(
                "run_command",
                &args,
                dir.path(),
                &super::super::PermissionRules::default(),
                &settings(),
            )
            .unwrap();
        let error = shell
            .spawn(
                &registry,
                "run_command",
                args,
                ToolContext::new().with(receipt),
            )
            .await
            .unwrap()
            .unwrap_err();
        assert!(called.load(Ordering::SeqCst));
        assert!(error.to_string().contains("replacement refuses"));
        assert!(!dir.path().join("marker").exists());
        executor.stop_all().await;
    }

    #[test]
    fn directories_outside_the_root_and_unavailable_shell_receipts_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let (shell, _, _) = shell(dir.path());
        for args in [
            json!({"command":"echo ok", "working_dir": outside.path()}),
            json!({"command":"echo ok", "working_dir":".."}),
            json!({"command":"echo ok", "working_dir":42}),
        ] {
            assert!(call_directory(&args, dir.path()).is_none());
            assert!(
                shell
                    .receipt(
                        "run_command",
                        &args,
                        dir.path(),
                        &super::super::PermissionRules::default(),
                        &settings()
                    )
                    .is_none()
            );
        }
        let args = json!({"command":"echo ok"});
        assert!(
            shell
                .receipt(
                    "read_file",
                    &args,
                    dir.path(),
                    &super::super::PermissionRules::default(),
                    &settings()
                )
                .is_none()
        );
        assert!(
            ShellApproval::default()
                .receipt(
                    "run_command",
                    &args,
                    dir.path(),
                    &super::super::PermissionRules::default(),
                    &settings()
                )
                .is_none()
        );
    }

    #[tokio::test]
    async fn denial_and_prompt_failures_have_distinct_typed_results() {
        struct Broken(bool);
        impl IOProvider for Broken {
            async fn send(&self, _message: IOMessage) -> Result<(), IOError> {
                if self.0 { Err(IOError::Closed) } else { Ok(()) }
            }
            async fn receive(&self) -> Result<IOMessage, IOError> {
                Err(IOError::Closed)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let request = ApprovalRequest::new("read_file", &json!({}), dir.path(), 0, false);
        let grants = SessionGrants::default();
        let (denied, _) = io("deny", false);
        assert!(matches!(
            request.ask(&denied, "Read?".into(), &grants).await,
            Err(ApprovalError::Denied { .. })
        ));
        for send_fails in [true, false] {
            let error = request
                .ask(
                    &UserIO::new(Arc::new(Broken(send_fails))),
                    "Read?".into(),
                    &grants,
                )
                .await
                .unwrap_err();
            assert!(matches!(error, ApprovalError::Prompt(IOError::Closed)));
            assert!(std::error::Error::source(&error).is_some());
        }
    }
    #[tokio::test]
    async fn interrupted_prompt_is_resolved_with_its_original_request_id() {
        struct Pending(tokio::sync::mpsc::UnboundedSender<IOMessage>);
        impl IOProvider for Pending {
            async fn send(&self, message: IOMessage) -> Result<(), IOError> {
                self.0.send(message).map_err(|_| IOError::Closed)
            }
            async fn receive(&self) -> Result<IOMessage, IOError> {
                std::future::pending().await
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let args = json!({"command":"echo ok"});
        let request = ApprovalRequest::new("run_command", &args, directory.path(), 1, true);
        let grants = SessionGrants::default();
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let io = UserIO::new(Arc::new(Pending(sent)));
        let prompt = {
            let mut waiting = std::pin::pin!(request.ask(&io, "Run?".into(), &grants));
            tokio::select! {
                message = received.recv() => message.unwrap(),
                _ = &mut waiting => panic!("no answer was supplied"),
            }
        };
        request.cancel(&io).await.unwrap();
        let resolved = received.recv().await.unwrap();
        assert_eq!(
            prompt.metadata.get(IO_TYPE).map(String::as_str),
            Some(IO_TYPE_CONFIRM)
        );
        assert_eq!(
            resolved.metadata.get(IO_TYPE).map(String::as_str),
            Some(IO_TYPE_APPROVAL_RESOLVED)
        );
        assert_eq!(
            resolved.metadata.get(APPROVAL_REQUEST_ID),
            prompt.metadata.get(APPROVAL_REQUEST_ID)
        );
        assert!(!grants.allows(1, "run_command", &args, directory.path()));
    }
    #[tokio::test]
    async fn an_answer_identifying_another_prompt_cannot_create_a_grant() {
        let directory = tempfile::tempdir().unwrap();
        let args = json!({"command":"echo ok"});
        let request = ApprovalRequest::new("run_command", &args, directory.path(), 1, true);
        let grants = SessionGrants::default();
        let io = UserIO::new(Arc::new(Answer {
            reply: IOMessage::user_text("exact")
                .with_metadata(CONFIRMED, CONFIRMED_TRUE)
                .with_metadata(APPROVAL_CHOICE, "exact")
                .with_metadata(APPROVAL_REQUEST_ID, "stale"),
            prompts: Mutex::new(Vec::new()),
        }));
        assert!(matches!(
            request.ask(&io, "Run?".into(), &grants).await,
            Err(ApprovalError::Denied { .. })
        ));
        assert!(!grants.allows(1, "run_command", &args, directory.path()));
    }
}
