//! Shared value contracts for workflow graph systems.
use super::io::{FlowError, FlowResult, dollars, parse_micros, relative};
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
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
/// A dollar amount held as exact micro-dollars.
///
/// It reads from a TOML or JSON number or string with up to six fractional
/// digits and writes as a decimal string, so no value passes through a binary
/// fraction on its way to the ledger.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct UsdAmount(Micros);
impl UsdAmount {
    /// Wraps an exact number of micro-dollars.
    pub const fn from_micros(micros: Micros) -> Self {
        Self(micros)
    }
    /// The amount in micro-dollars.
    pub const fn micros(self) -> Micros {
        self.0
    }
}
impl std::str::FromStr for UsdAmount {
    type Err = FlowError;
    fn from_str(text: &str) -> Result<Self, FlowError> {
        parse_micros(text).map(Self)
    }
}
impl std::fmt::Display for UsdAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        dollars(self.0).fmt(f)
    }
}
impl Serialize for UsdAmount {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for UsdAmount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Amount;
        impl de::Visitor<'_> for Amount {
            type Value = UsdAmount;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a dollar amount given as a number or a string")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<UsdAmount, E> {
                text.parse().map_err(E::custom)
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<UsdAmount, E> {
                self.visit_str(&value.to_string())
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<UsdAmount, E> {
                self.visit_str(&value.to_string())
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<UsdAmount, E> {
                self.visit_str(&value.to_string())
            }
        }
        deserializer.deserialize_any(Amount)
    }
}
/// Durable, relative step identifier.
///
/// A key is a `/`-separated relative path. Empty, `.` and `..` segments are
/// rejected, and so are segments the journal keeps for itself: all-digit
/// segments name numbered workspaces and `_abandoned` names the abandoned
/// directory. Deserializing validates the same way.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StepKey(String);
impl StepKey {
    /// Creates a key from a relative name.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when the name is not a safe relative
    /// path or contains a reserved segment.
    pub fn new(key: impl Into<String>) -> FlowResult<Self> {
        let key = key.into();
        relative(&key)?;
        if let Some(segment) = key
            .split('/')
            .find(|s| *s == "_abandoned" || s.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(FlowError::invalid(format!(
                "reserved segment `{segment}` in step key: {key}"
            )));
        }
        Ok(Self(key))
    }
    /// Appends a relative segment.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when the extended key is not valid.
    pub fn child(&self, seg: &str) -> FlowResult<Self> {
        Self::new(format!("{}/{seg}", self.0))
    }
    /// Borrows the key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for StepKey {
    type Error = FlowError;
    fn try_from(key: String) -> FlowResult<Self> {
        Self::new(key)
    }
}
impl TryFrom<&str> for StepKey {
    type Error = FlowError;
    fn try_from(key: &str) -> FlowResult<Self> {
        Self::new(key)
    }
}
impl From<StepKey> for String {
    fn from(key: StepKey) -> Self {
        key.0
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[non_exhaustive]
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
impl StopCause {
    /// Ledger status for a measured runner stop.
    pub(crate) fn ledger_status(&self) -> &'static str {
        match self {
            Self::Finished => "completed",
            Self::TurnCap => "turn_cap",
            Self::CostCap => "cost_cap",
            Self::Timeout => "timeout",
            Self::Interrupted => "interrupted",
            Self::ModelError(_) => "failed",
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
/// Workflow value: `Usage`.
pub struct Usage {
    /// Name of the service that served the model. When empty, the ledger
    /// falls back to the provider in its settings.
    pub provider: String,
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
/// Why a role call failed. Only [`Setup`](Self::Setup) failures happen before
/// the model is reached, so only they are settled at no cost.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentFailure {
    /// The call could not start; nothing was spent.
    Setup(String),
    /// The call ran but its cost could not be measured.
    Unmeasured(String),
    /// The workflow failed to handle the call.
    Internal(String),
}
impl std::fmt::Display for AgentFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(why) => write!(f, "agent setup failed: {why}"),
            Self::Unmeasured(why) => write!(f, "agent cost was not measured: {why}"),
            Self::Internal(why) => write!(f, "agent call failed: {why}"),
        }
    }
}
impl std::error::Error for AgentFailure {}
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[non_exhaustive]
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
///
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
#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<StopState>);
#[derive(Debug, Default)]
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
#[non_exhaustive]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// The most one call of this role may spend. It reads from a number or a
    /// string.
    pub max_usd: UsdAmount,
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
///
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
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoleBook {
    /// Roles.
    pub roles: BTreeMap<String, Role>,
}
impl GlobalResource for RoleBook {}
/// Whether a workflow starts over or picks up where an earlier run stopped.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RunMode {
    /// A new run: every step executes.
    #[default]
    Run,
    /// A restart: a step journaled as done is restored instead of run again.
    /// Steps that were skipped, failed or interrupted run again.
    Resume,
}
/// Identity and paths of the current durable workflow.
///
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunHandle {
    /// Run id.
    pub run_id: String,
    /// Root.
    pub root: PathBuf,
    /// Whether this run is new or resumes an earlier one.
    pub mode: RunMode,
}
impl GlobalResource for RunHandle {}
/// Injected executor shared by graph systems.
///
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
/// async fn inspect(value:Res<AgentRunner>)->std::sync::Arc<dyn polaris_cosmos::flow::TaskRunner> { value.runner().clone() }
/// ```
#[derive(Clone)]
pub struct AgentRunner(Arc<dyn TaskRunner>);
impl AgentRunner {
    /// Wraps the executor that every call in the workflow uses.
    pub fn new(runner: Arc<dyn TaskRunner>) -> Self {
        Self(runner)
    }
    /// The wrapped executor.
    pub fn runner(&self) -> &Arc<dyn TaskRunner> {
        &self.0
    }
}
impl std::fmt::Debug for AgentRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRunner").finish_non_exhaustive()
    }
}
impl GlobalResource for AgentRunner {}
