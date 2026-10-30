//! Host-issued shell authorization, consumed only through registered tools.
//!
//! An embedding host issues an [`ApprovalReceipt`] after its own authorization
//! decision and inserts it into [`ToolContext`]. JSON arguments cannot create
//! context values. Validation belongs to the host; execution and receipt
//! consumption belong to the shell capability.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use polaris::models::llm::ToolDefinition;
use polaris::shell::{
    ShellCancel, ShellExecutor, ShellPlugin, ShellRequest, ShellToolResponse, ShellTools,
};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
use polaris::system::server::Server;
use polaris::tools::{Tool, ToolContext, ToolError, ToolRegistry};
use serde::Deserialize;
use serde_json::Value;

/// Single-use authorization for exact tool arguments and a canonical directory.
///
/// Only trusted Rust host code can issue this context value. The validator must
/// check that the policy revision and session generation captured at issuance
/// are still current. Clones share consumption: at most one execution succeeds.
#[derive(Clone)]
pub struct ApprovalReceipt {
    tool: String,
    args: Value,
    directory: PathBuf,
    root: PathBuf,
    valid: Arc<dyn Fn() -> bool + Send + Sync>,
    consumed: Arc<AtomicBool>,
}

impl std::fmt::Debug for ApprovalReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalReceipt")
            .field("tool", &self.tool)
            .field("directory", &self.directory)
            .field("root", &self.root)
            .field("consumed", &self.consumed)
            .finish_non_exhaustive()
    }
}

impl ApprovalReceipt {
    /// Issue authorization after a host decision, binding the displayed directory.
    ///
    /// `directory` must be the canonical directory captured before approval,
    /// so changing a symlink while the prompt is open invalidates the receipt.
    /// `valid` must recheck the host's captured rule revision and session generation.
    /// A changed or invalid directory yields a receipt that cannot be consumed;
    /// issuance never falls back to an ordinary, unapproved call.
    #[must_use]
    pub fn issue(
        tool: &str,
        args: Value,
        root: &Path,
        directory: &Path,
        valid: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Self {
        let root = call_paths(&args, root).map_or_else(|| root.to_path_buf(), |(root, _)| root);
        Self {
            tool: tool.into(),
            args,
            root,
            directory: directory.into(),
            valid: Arc::new(valid),
            consumed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Check the original JSON before parameter decoding can discard fields.
    #[must_use]
    pub fn matches(&self, tool: &str, args: &Value) -> bool {
        self.tool == tool
            && self.args == *args
            && call_directory(args, &self.root).as_ref() == Some(&self.directory)
            && (self.valid)()
            && !self.consumed.load(Ordering::SeqCst)
    }

    /// Consume once immediately before execution.
    #[must_use]
    pub fn consume(&self, tool: &str, args: &Value) -> bool {
        self.matches(tool, args) && !self.consumed.swap(true, Ordering::SeqCst)
    }

    /// Consume once and obtain the exact canonical execution directory.
    #[must_use]
    pub fn consume_directory(&self, tool: &str, args: &Value) -> Option<PathBuf> {
        self.consume(tool, args).then(|| self.directory.clone())
    }

    /// Consume the background tool's decoded arguments without accepting extra fields.
    #[must_use]
    pub fn consume_background(&self, command: &str, timeout: Option<u64>) -> Option<PathBuf> {
        if !self.args.as_object().is_some_and(|args| {
            args.keys()
                .all(|key| key == "command" || key == "timeout_secs")
        }) || self.args.get("command").and_then(Value::as_str) != Some(command)
            || self.args.get("timeout_secs").and_then(Value::as_u64) != timeout
        {
            return None;
        }
        self.consume_directory("run_in_background", &self.args)
    }
}

/// Resolve an existing command directory and enforce canonical root containment.
pub(crate) fn call_directory(args: &Value, root: &Path) -> Option<PathBuf> {
    call_paths(args, root).map(|(_, directory)| directory)
}

/// Capture canonical root and command directory with one metadata lookup path.
pub(crate) fn call_paths(args: &Value, root: &Path) -> Option<(PathBuf, PathBuf)> {
    let root = root.canonicalize().ok()?;
    let requested = match args.get("working_dir") {
        None | Some(Value::Null) => root.clone(),
        Some(Value::String(dir)) => root.join(dir),
        _ => return None,
    };
    let directory = requested.canonicalize().ok()?;
    directory.starts_with(&root).then_some((root, directory))
}

/// Registered foreground shell tool with optional host approval context.
///
/// Ordinary calls delegate to the framework's shell tool. Approved calls decode
/// the same arguments, consume their receipt, then use the existing executor;
/// hard denies, sandbox checks, cancellation and output sinks still apply.
pub struct ApprovalShellTool {
    executor: ShellExecutor,
    ordinary: Arc<dyn Tool>,
}

impl std::fmt::Debug for ApprovalShellTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalShellTool")
            .field("executor", &self.executor)
            .finish_non_exhaustive()
    }
}

impl ApprovalShellTool {
    /// Adapt the framework's `run_command` over a shared executor.
    ///
    /// # Panics
    ///
    /// Panics if the framework's `ShellTools` violates its `run_command`
    /// registration contract.
    #[must_use]
    pub fn new(executor: ShellExecutor) -> Self {
        let mut registry = ToolRegistry::new();
        registry.register_toolset(ShellTools::new(executor.clone()));
        Self {
            executor,
            ordinary: registry
                .to_arc("run_command")
                .expect("ShellTools registers run_command"),
        }
    }
}

impl Tool for ApprovalShellTool {
    fn definition(&self) -> ToolDefinition {
        self.ordinary.definition()
    }
    fn permission(&self) -> polaris::tools::ToolPermission {
        self.ordinary.permission()
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + 'a>>
    {
        Box::pin(async move {
            let Some(receipt) = context.get::<ApprovalReceipt>() else {
                return self.ordinary.execute(args, context).await;
            };
            let decoded: ForegroundArgs = serde_json::from_value(args.clone())
                .map_err(|err| ToolError::parameter_error(err.to_string()))?;
            let Some(directory) = receipt.consume_directory("run_command", &args) else {
                return Err(ToolError::permission_denied(
                    "Approval does not match this invocation",
                ));
            };
            let _ = decoded.working_dir;
            let mut request = ShellRequest::new(decoded.command).with_working_dir(directory);
            if let Some(timeout) = decoded.timeout_secs {
                request = request.with_timeout(timeout);
            }
            if let Some(cancel) = context.get::<ShellCancel>() {
                request = request.with_cancel(cancel.clone());
            }
            if let Some(sink) = context.get::<polaris::shell::ShellSinkHandle>() {
                request = request.with_sink(sink.clone());
            }
            let result =
                self.executor
                    .execute_confirmed(request)
                    .await
                    .map_err(|err| match err {
                        polaris::shell::ShellError::PermissionDenied(command) => {
                            ToolError::permission_denied(format!(
                                "Command denied by policy: {command}"
                            ))
                        }
                        other => ToolError::execution_error(other.to_string()),
                    })?;
            serde_json::to_value(ShellToolResponse::Executed(result))
                .map_err(|err| ToolError::execution_error(err.to_string()))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForegroundArgs {
    command: String,
    working_dir: Option<String>,
    timeout_secs: Option<u64>,
}

/// Adds receipt-aware foreground execution to the framework shell capability.
///
/// Install alongside [`ShellPlugin`], which owns the executor and its cleanup.
/// This plugin replaces its standard `run_command` registration during `build`;
/// every subsequent dispatch invokes the registered [`ApprovalShellTool`].
///
/// # Resources Provided
///
/// None. Extends the existing [`ToolRegistry`] and consumes [`ShellExecutor`].
///
/// # Dependencies
///
/// - [`ShellPlugin`] — publishes the managed executor and standard shell tool.
/// - [`polaris::tools::ToolsPlugin`] — provides the tool registry.
///
/// # Lifecycle
///
/// `build` rebuilds the mutable registry with a receipt-aware `run_command`,
/// preserving registered tool order, effective schemas, permissions and exposure.
/// Other tools retain their implementations. No processes or tasks are started;
/// the framework shell plugin remains responsible for cleanup.
///
/// # Example
///
/// ```no_run
/// use polaris::shell::{ShellConfig, ShellPlugin};
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code::tools::approval::ApprovalShellPlugin;
/// let mut server = Server::new();
/// server.add_plugins(ToolsPlugin)
///     .add_plugins(ShellPlugin::new(ShellConfig::new()))
///     .add_plugins(ApprovalShellPlugin);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ApprovalShellPlugin;

impl Plugin for ApprovalShellPlugin {
    const ID: &'static str = "polaris_code::shell_approval";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ShellPlugin>()]
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .extends::<ToolRegistry>(VersionReq::caret(ToolRegistry::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        let Some(executor) = server
            .get_global::<ShellExecutor>()
            .map(|value| value.clone())
        else {
            tracing::error!("shell executor unavailable for approval registration");
            return;
        };
        let Some(mut registry) = server.get_resource_mut::<ToolRegistry>() else {
            tracing::error!("mutable tool registry unavailable for approval registration");
            return;
        };
        match approval_registry(&mut registry, executor) {
            Ok(rebuilt) => *registry = rebuilt,
            Err(error) => tracing::error!(%error, "could not register approved shell execution"),
        }
    }
}

// ToolRegistry supports registry reconstruction through public lookups rather
// than in-place replacement. Retain effective definitions (including strict
// overrides) and forward every other tool's execution unchanged.
struct RetainedTool {
    tool: Arc<dyn Tool>,
    definition: ToolDefinition,
}

impl Tool for RetainedTool {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
    fn permission(&self) -> polaris::tools::ToolPermission {
        self.tool.permission()
    }
    fn execute<'a>(
        &'a self,
        args: Value,
        context: &'a ToolContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + 'a>>
    {
        self.tool.execute(args, context)
    }
}

fn approval_registry(
    original: &mut ToolRegistry,
    executor: ShellExecutor,
) -> Result<ToolRegistry, ToolError> {
    let overrides: Vec<_> = original
        .permission_overrides()
        .iter()
        .map(|(name, permission)| (name.clone(), *permission))
        .collect();
    let mut rebuilt = ToolRegistry::new();
    for definition in original.all_definitions() {
        let name = definition.name.clone();
        let tool = if name == "run_command" {
            Arc::new(ApprovalShellTool::new(executor.clone())) as Arc<dyn Tool>
        } else {
            original
                .to_arc(&name)
                .ok_or_else(|| ToolError::unknown_tool(&name))?
        };
        // A denied tool is implicitly hidden. Probe exposure independently of
        // permission so a later build-time grant can still expose it, while an
        // explicit hidden setting survives that grant.
        let permission = original
            .permission(&name)
            .ok_or_else(|| ToolError::unknown_tool(&name))?;
        original.set_permission(&name, polaris::tools::ToolPermission::Allow)?;
        let exposed = original.is_exposed(&name);
        original.set_permission(&name, permission)?;
        rebuilt.register(RetainedTool { tool, definition });
        rebuilt.set_exposed(&name, exposed)?;
    }
    for (name, permission) in overrides {
        rebuilt.set_permission(&name, permission)?;
    }
    Ok(rebuilt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::shell::ShellConfig;
    use polaris::tools::ToolsPlugin;
    use serde_json::json;

    #[tokio::test]
    async fn composed_shell_provider_registers_and_consumes_host_authorization() {
        let directory = tempfile::tempdir().unwrap();
        let mut server = Server::new();
        server
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::new(
                ShellConfig::new()
                    .with_working_dir(directory.path())
                    .with_allowed_dirs(vec![directory.path().into()]),
            ))
            .add_plugins(ApprovalShellPlugin);
        server.finish().await.unwrap();
        let registry = server.get_global::<ToolRegistry>().unwrap();
        let executor = server.get_global::<ShellExecutor>().unwrap();
        let args = json!({"command":"printf x >> marker; echo \"$(pwd)\""});
        let ordinary = registry.execute("run_command", &args).await.unwrap();
        assert_eq!(ordinary["status"], "confirmation_required");
        assert!(!directory.path().join("marker").exists());
        let current = Arc::new(AtomicBool::new(true));
        let validator = current.clone();
        let receipt = ApprovalReceipt::issue(
            "run_command",
            args.clone(),
            directory.path(),
            &directory.path().canonicalize().unwrap(),
            move || validator.load(Ordering::SeqCst),
        );
        let context = ToolContext::new().with(receipt);
        assert_eq!(
            registry
                .execute_with("run_command", &args, &context)
                .await
                .unwrap()["status"],
            "executed"
        );
        assert!(
            registry
                .execute_with("run_command", &args, &context)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(directory.path().join("marker")).unwrap(),
            b"x"
        );
        let validator = current.clone();
        let receipt = ApprovalReceipt::issue(
            "run_command",
            args.clone(),
            directory.path(),
            &directory.path().canonicalize().unwrap(),
            move || validator.load(Ordering::SeqCst),
        );
        current.store(false, Ordering::SeqCst);
        assert!(
            registry
                .execute_with("run_command", &args, &ToolContext::new().with(receipt))
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(directory.path().join("marker")).unwrap(),
            b"x"
        );
        executor.stop_all().await;
    }

    #[tokio::test]
    async fn approval_registration_preserves_other_tools_and_registry_overrides() {
        use polaris::tools::ToolPermission;
        struct Other;
        impl Tool for Other {
            fn definition(&self) -> ToolDefinition {
                ToolDefinition::new("other", "Keep me", json!({"type":"object"})).with_strict(true)
            }
            fn execute<'a>(
                &'a self,
                args: Value,
                _: &'a ToolContext,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<Value, ToolError>> + Send + 'a>,
            > {
                Box::pin(async move { Ok(args) })
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let executor = ShellExecutor::new(ShellConfig::new().with_working_dir(directory.path()));
        let mut registry = ToolRegistry::new();
        registry.register_toolset(ShellTools::new(executor.clone()));
        registry.register(Other);
        registry.set_strict("other", false).unwrap();
        registry
            .set_permission("other", ToolPermission::Confirm)
            .unwrap();
        registry.set_exposed("other", false).unwrap();
        registry.set_strict("run_command", true).unwrap();
        registry
            .set_permission("run_command", ToolPermission::Deny)
            .unwrap();
        let mut rebuilt = approval_registry(&mut registry, executor.clone()).unwrap();
        assert_eq!(rebuilt.names(), registry.names());
        assert_eq!(
            rebuilt
                .all_definitions()
                .iter()
                .map(|def| (def.name.clone(), def.strict))
                .collect::<Vec<_>>(),
            registry
                .all_definitions()
                .iter()
                .map(|def| (def.name.clone(), def.strict))
                .collect::<Vec<_>>()
        );
        for name in registry.names() {
            assert_eq!(rebuilt.permission(name), registry.permission(name));
            assert_eq!(rebuilt.is_exposed(name), registry.is_exposed(name));
        }
        rebuilt
            .set_permission("run_command", ToolPermission::Allow)
            .unwrap();
        assert!(
            rebuilt.is_exposed("run_command"),
            "an implicit deny must not become an explicit hide"
        );
        rebuilt
            .set_permission("other", ToolPermission::Allow)
            .unwrap();
        assert!(
            !rebuilt.is_exposed("other"),
            "an explicit hide survives a permission grant"
        );
        assert_eq!(
            rebuilt
                .execute("other", &json!({"unchanged":true}))
                .await
                .unwrap(),
            json!({"unchanged":true})
        );
        executor.stop_all().await;
    }

    #[tokio::test]
    async fn model_arguments_cannot_supply_approval_context() {
        let directory = tempfile::tempdir().unwrap();
        let executor = ShellExecutor::new(ShellConfig::new().with_working_dir(directory.path()));
        let tool = ApprovalShellTool::new(executor.clone());
        let result = tool.execute(json!({"command":"touch marker; echo \"$(pwd)\"", "approval":true, "confirmed":true}), &ToolContext::new()).await.unwrap();
        assert_eq!(result["status"], "confirmation_required");
        assert!(!directory.path().join("marker").exists());
        executor.stop_all().await;
    }
}
