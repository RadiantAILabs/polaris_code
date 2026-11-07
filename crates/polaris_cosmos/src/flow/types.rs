//! Shared value contracts for workflow graph systems.
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
/// Integer millionths of one dollar.
pub type Micros = u64;
/// Durable, relative step identifier.
#[derive(Clone, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
/// Workflow value: `StepKey`.
pub struct StepKey(pub String);
impl StepKey {
    /// Creates a key from a relative name.
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }
    /// Appends a relative segment.
    pub fn child(&self, seg: &str) -> Self {
        Self(format!("{}/{seg}", self.0))
    }
    /// Borrows the key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl From<&str> for StepKey {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}
impl std::fmt::Display for StepKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
/// Replaceable executor for one isolated agent session.
#[async_trait::async_trait]
pub trait TaskRunner: Send + Sync {
    /// Runs exactly one role call.
    async fn run(&self, req: AgentRequest) -> Result<AgentReport, AgentFailure>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentRequest`.
pub struct AgentRequest {
    /// Step.
    pub step: StepKey,
    /// Role.
    pub role: String,
    /// Workspace.
    pub workspace: PathBuf,
    /// System prompt.
    pub system_prompt: String,
    /// Turn prompt.
    pub turn_prompt: String,
    /// Limits.
    pub limits: AgentLimits,
    /// Grants.
    pub grants: ToolGrant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentLimits`.
pub struct AgentLimits {
    /// Max turns.
    pub max_turns: u32,
    /// Max micros.
    pub max_micros: Micros,
    /// Timeout.
    pub timeout: Duration,
    /// The most tokens one model reply of the call may hold. `None` leaves
    /// the runner's own setting in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Workflow value: `ToolGrant`.
pub struct ToolGrant {
    /// Edit.
    pub edit: bool,
    /// Commands.
    pub commands: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentReport`.
pub struct AgentReport {
    /// Stop.
    pub stop: StopCause,
    /// Usage.
    pub usage: Usage,
    /// Final text.
    pub final_text: String,
    /// Transcript.
    pub transcript: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Workflow value: `StopCause`.
pub enum StopCause {
    /// Finished outcome.
    Finished,
    /// `TurnCap` outcome.
    TurnCap,
    /// `CostCap` outcome.
    CostCap,
    /// Timeout outcome.
    Timeout,
    /// Interrupted outcome.
    Interrupted,
    /// `ModelError` outcome.
    ModelError(String),
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
/// Workflow value: `Usage`.
pub struct Usage {
    /// Model id.
    pub model_id: String,
    /// Input.
    pub input: u64,
    /// Output.
    pub output: u64,
    /// Cache read.
    pub cache_read: u64,
    /// Cache write.
    pub cache_write: u64,
    /// Requests.
    pub requests: u32,
    /// Micros.
    pub micros: Micros,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentFailure`.
pub enum AgentFailure {
    /// Setup outcome.
    Setup(String),
    /// Unmeasured outcome.
    Unmeasured(String),
    /// Internal outcome.
    Internal(String),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentCall`.
pub struct AgentCall {
    /// Step.
    pub step: StepKey,
    /// Role.
    pub role: String,
    /// Phase.
    pub phase: String,
    /// Stage.
    pub stage: StageSpec,
    /// Expect.
    pub expect: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Workflow value: `StageSpec`.
pub struct StageSpec {
    /// Copy in.
    pub copy_in: Vec<(PathBuf, String)>,
    /// Vars.
    pub vars: BTreeMap<String, String>,
    /// Turn prompt.
    pub turn_prompt: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `AgentOutcome`.
pub enum AgentOutcome {
    /// A completed invocation, including bounded stop outcomes.
    Ran {
        /// Measured runner report.
        report: AgentReport,
        /// Expected ordinary files collected after settlement.
        outputs: Vec<OutputFile>,
    },
    /// Failed outcome.
    Failed(AgentFailure),
    /// Skipped outcome.
    Skipped(SkipReason),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `OutputFile`.
pub struct OutputFile {
    /// Path.
    pub path: String,
    /// Sha256.
    pub sha256: String,
    /// Bytes.
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Workflow value: `SkipReason`.
pub enum SkipReason {
    /// Budget outcome.
    Budget,
    /// Disabled outcome.
    Disabled,
    /// `NotApplicable` outcome.
    NotApplicable,
}
/// Cooperative stop request shared by every call in a workflow.
#[derive(Clone, Default)]
/// Workflow value: `StopFlag`.
///
/// # Scope
/// Global because the same stop request is observed by every call.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<StopFlag>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::StopFlag;
/// #[system]
/// async fn inspect(value:Res<StopFlag>)->bool { value.is_requested() }
/// ```
pub struct StopFlag(Arc<StopState>);
#[derive(Default)]
struct StopState {
    set: AtomicBool,
    notify: tokio::sync::Notify,
}
impl StopFlag {
    /// Requests stop and wakes every waiter.
    pub fn request(&self) {
        self.0.set.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }
    /// Whether stop has been requested.
    pub fn is_requested(&self) -> bool {
        self.0.set.load(Ordering::SeqCst)
    }
    /// Waits until stop is requested, including an earlier request.
    pub async fn requested(&self) {
        loop {
            let wait = self.0.notify.notified();
            tokio::pin!(wait);
            wait.as_mut().enable();
            if self.is_requested() {
                return;
            }
            wait.await;
        }
    }
}
impl GlobalResource for StopFlag {}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
/// Workflow value: `PhaseStatus`.
pub enum PhaseStatus {
    /// Completed outcome.
    Completed,
    /// Degraded outcome.
    Degraded(String),
    /// Failed outcome.
    Failed(String),
    /// Skipped outcome.
    Skipped(String),
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
/// Workflow value: `PhasePolicy`.
pub enum PhasePolicy {
    /// Fatal outcome.
    Fatal,
    /// Degrade outcome.
    Degrade,
    /// Incomplete outcome.
    Incomplete,
}
/// A role's immutable prompt and execution policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Workflow value: `Role`.
pub struct Role {
    /// Prompt.
    pub prompt: String,
    /// Node.
    pub node: String,
    /// Max turns.
    pub max_turns: u32,
    /// Max usd.
    pub max_usd: toml::Value,
    /// Timeout seconds.
    pub timeout_seconds: u64,
    /// The most tokens one model reply of this role may hold. A role that
    /// writes long files in one reply sets it above the runner's default;
    /// when absent, the runner's own setting applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Shell.
    pub shell: Vec<String>,
    /// Read only.
    pub read_only: bool,
    /// Skills.
    pub skills: Vec<String>,
}
/// Resolved roles shared by all workflow branches.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// Workflow value: `RoleBook`.
///
/// # Scope
/// Global because all role calls use the same resolved policies.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<RoleBook>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RoleBook;
/// #[system]
/// async fn inspect(value:Res<RoleBook>)->usize { value.roles.len() }
/// ```
pub struct RoleBook {
    /// Roles.
    pub roles: BTreeMap<String, Role>,
}
impl GlobalResource for RoleBook {}
/// Identity and paths of the current durable workflow.
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Workflow value: `RunHandle`.
///
/// # Scope
/// Global because every branch writes inside the same durable run.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<RunHandle>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RunHandle;
/// #[system]
/// async fn inspect(value:Res<RunHandle>)->std::path::PathBuf { value.root.clone() }
/// ```
pub struct RunHandle {
    /// Run id.
    pub run_id: String,
    /// Root.
    pub root: PathBuf,
    /// Mode.
    pub mode: String,
}
impl GlobalResource for RunHandle {}
/// Injected executor shared by graph systems.
#[derive(Clone)]
/// Workflow value: `AgentRunner`.
///
/// # Scope
/// Global because concurrent graph branches share one replaceable executor.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<AgentRunner>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::AgentRunner;
/// #[system]
/// async fn inspect(value:Res<AgentRunner>)->std::sync::Arc<dyn polaris_cosmos::flow::TaskRunner> { value.0.clone() }
/// ```
pub struct AgentRunner(pub Arc<dyn TaskRunner>);
impl GlobalResource for AgentRunner {}
