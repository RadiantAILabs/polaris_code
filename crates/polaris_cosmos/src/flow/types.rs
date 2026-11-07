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
/// One role call handed to a [`TaskRunner`]: the prompts, the workspace, the
/// limits and the tool grants it runs with.
pub struct AgentRequest {
    /// Durable step the call belongs to.
    pub step: StepKey,
    /// Name of the role, a key of the [`RoleBook`].
    pub role: String,
    /// Directory the call works in. It holds the `inputs/` the call may read and
    /// the `outputs/` it writes.
    pub workspace: PathBuf,
    /// Rendered system prompt.
    pub system_prompt: String,
    /// Rendered prompt of the first turn.
    pub turn_prompt: String,
    /// Ceilings the runner must keep the call within.
    pub limits: AgentLimits,
    /// Tools the call may use.
    pub grants: ToolGrant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
/// Ceilings a runner keeps one call within.
pub struct AgentLimits {
    /// Most model turns the call may take.
    pub max_turns: u32,
    /// Most the call may cost, in micro-dollars. This is the amount reserved in
    /// the [`SpendBook`](super::SpendBook) for the call.
    pub max_micros: Micros,
    /// Wall-clock limit for the whole call. It is the role's timeout, cut down
    /// to the time left in the phase when the phase has a deadline.
    pub timeout: Duration,
    /// The most tokens one model reply of the call may hold. `None` leaves
    /// the runner's own setting in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// The tools a call may use.
pub struct ToolGrant {
    /// Whether the call may edit files.
    pub edit: bool,
    /// Shell commands the call may run.
    pub commands: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
/// What a runner measured for a call that ran to a stop.
pub struct AgentReport {
    /// Why the call stopped.
    pub stop: StopCause,
    /// Measured usage and cost.
    pub usage: Usage,
    /// The model's last reply.
    pub final_text: String,
    /// Path of the saved transcript, when the runner kept one.
    pub transcript: Option<PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
/// Why a call that reached the model stopped.
pub enum StopCause {
    /// The model finished on its own.
    Finished,
    /// The call reached its turn limit.
    TurnCap,
    /// The call reached its cost ceiling.
    CostCap,
    /// The call reached its wall-clock limit.
    Timeout,
    /// A user interruption ended the call.
    Interrupted,
    /// The model service failed; carries the error text.
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
/// Tokens, requests and cost measured for one call.
pub struct Usage {
    /// Name of the service that served the model. When empty, the ledger
    /// falls back to the provider in its settings.
    pub provider: String,
    /// Identifier of the model that served the call.
    pub model_id: String,
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Input tokens read from the prompt cache.
    pub cache_read: u64,
    /// Input tokens written to the prompt cache.
    pub cache_write: u64,
    /// Number of model requests the call made.
    pub requests: u32,
    /// Cost of the call in micro-dollars, millionths of a dollar. The
    /// [`SpendBook`](super::SpendBook) settles this amount.
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
/// One role call a graph asks the [`agent`](fn@super::agent) block to make.
pub struct AgentCall {
    /// Durable step the call is recorded under.
    pub step: StepKey,
    /// Name of the role, a key of the [`RoleBook`].
    pub role: String,
    /// Phase the call is checked against and charged to.
    pub phase: String,
    /// What to put in the call's workspace before it runs.
    pub stage: StageSpec,
    /// Paths, relative to the call's workspace, of the files the call should
    /// write. Each one that exists afterwards is hashed and returned in
    /// [`AgentOutcome::Ran`]; one that is missing is left out.
    pub expect: Vec<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
/// What a call's workspace holds before the call runs.
pub struct StageSpec {
    /// Files or directories to copy in, as pairs of source path and name under
    /// the workspace's `inputs/`. A source must lie inside the run root, the task
    /// directory or the asset root.
    pub copy_in: Vec<(PathBuf, String)>,
    /// Values for the `{{name}}` placeholders of the prompts. A variable that no
    /// prompt uses is an error.
    pub vars: BTreeMap<String, String>,
    /// Prompt of the first turn. When absent, a default asks the model to do the
    /// task in its instructions and write results to `outputs/`.
    pub turn_prompt: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
/// How one role call ended.
pub enum AgentOutcome {
    /// A completed invocation, including bounded stop outcomes.
    Ran {
        /// Measured runner report.
        report: AgentReport,
        /// Expected ordinary files collected after settlement.
        outputs: Vec<OutputFile>,
    },
    /// The call failed; carries the failure.
    Failed(AgentFailure),
    /// The call was not made; carries the reason.
    Skipped(SkipReason),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
/// An expected output file a call left behind.
pub struct OutputFile {
    /// Path relative to the call's workspace, as listed in
    /// [`AgentCall::expect`].
    pub path: String,
    /// Hex SHA-256 digest of the file contents.
    pub sha256: String,
    /// Size of the file in bytes.
    pub bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
/// Why a call was not made.
pub enum SkipReason {
    /// The phase allowance could not cover the minimum grant, or the
    /// [`CallDeadline`](super::CallDeadline) has passed.
    Budget,
    /// The phase is halted or past its deadline.
    Disabled,
    /// The call does not apply to this run. No block in this module produces
    /// it; a host's own systems may.
    NotApplicable,
}
/// Cooperative stop request; use `Res<StopFlag>` when a system or runner should
/// end its work early once a stop has been asked for, or wake the moment one is.
///
/// No block in this module reads the flag. The host raises it, for example on an
/// interrupt signal, and a [`TaskRunner`] or the host's own systems watch it.
///
/// # Scope
///
/// **Global.** Every call in the workflow observes the same stop request.
///
/// # Provided by
///
/// Consumer-supplied. The host creates it with `StopFlag::default()` and
/// registers it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). It hands a
/// clone to the runner that must observe it, as
/// [`ScriptedRunner::with_stop`](super::ScriptedRunner::with_stop) does.
///
/// # Access
///
/// `Res<StopFlag>` only. The value is not immutable: `request` raises the flag
/// through a shared reference. A clone shares one `Arc` that holds an atomic
/// flag and a `tokio::sync::Notify`, so every clone sees the request and every
/// waiter wakes. There is no lock, and a raised flag cannot be lowered.
///
/// # Alternatives
///
/// [`PhaseControl`](super::PhaseControl) records an interruption that the graph
/// blocks honor: [`agent`](fn@super::agent) starts no new call once a phase is
/// halted. This flag only tells running code to stop.
///
/// # Example system
///
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
/// How a phase ended.
pub enum PhaseStatus {
    /// The phase finished without a failure.
    Completed,
    /// The phase failed, but its policy let the workflow go on; carries the
    /// failure note.
    Degraded(String),
    /// The phase failed; carries the failure note.
    Failed(String),
    /// The phase did not run; carries the reason.
    Skipped(String),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
/// What a phase failure means for the rest of the workflow.
pub enum PhasePolicy {
    /// A failure halts the workflow: no later phase starts.
    Fatal,
    /// A failure is recorded as `Degraded` and the workflow goes on.
    Degrade,
    /// A failure is recorded as `Failed`, the workflow goes on, and the run is
    /// marked incomplete.
    Incomplete,
}
/// A role's immutable prompt and execution policy.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// Prompt file of the role, relative to the `prompts` folder of the asset
    /// root and without its `.md` extension.
    pub prompt: String,
    /// Name of the execution node the role runs on. The flow blocks never read
    /// it; it is there for a host's runner to interpret.
    pub node: String,
    /// Most model turns one call of this role may take.
    pub max_turns: u32,
    /// The most one call of this role may spend, in dollars. It reads from a
    /// number or a string, and [`max_micros`](Self::max_micros) gives the exact
    /// micro-dollar amount.
    pub max_usd: UsdAmount,
    /// Wall-clock limit of one call of this role, in seconds.
    pub timeout_seconds: u64,
    /// The most tokens one model reply of this role may hold. A role that
    /// writes long files in one reply sets it above the runner's default;
    /// when absent, the runner's own setting applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Shell commands a call of this role may run.
    pub shell: Vec<String>,
    /// Whether a call of this role may not edit files.
    pub read_only: bool,
    /// Skill paths, relative to the `prompts` folder of the asset root. Each is
    /// copied into the call's workspace under `skills/`, by its file name.
    pub skills: Vec<String>,
}
/// Resolved role policies; use `Res<RoleBook>` when a system needs a role's
/// prompt, limits, spend ceiling or tool grants by name.
///
/// # Scope
///
/// **Global.** Every role call in the workflow uses the same resolved policies.
///
/// # Provided by
///
/// Consumer-supplied. The host builds it with [`RoleBook::load`], which merges a
/// directory of TOML role fragments, or fills the public `roles` map itself, and
/// registers it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`agent`](fn@super::agent) block reads it.
///
/// # Access
///
/// `Res<RoleBook>` only. The value is immutable: it is a plain map of roles with
/// no interior mutability, and nothing changes it after the host registers it.
///
/// # Alternatives
///
/// [`FlowSettings`](super::FlowSettings) holds the settings that are the same
/// for every role, such as the asset root. Use an ordinary local resource when
/// one branch needs a different role set.
///
/// # Serialization
///
/// Each TOML fragment read by [`load`](Self::load) holds one `[roles.<name>]`
/// table per role. [`write`](Self::write) saves the merged book to one TOML file
/// in the same shape, so a later `load` of that file gives the same roles. The
/// run root does not keep a copy of it.
///
/// # Example system
///
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RoleBook;
/// #[system]
/// async fn inspect(value:Res<RoleBook>)->usize { value.roles.len() }
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RoleBook {
    /// Roles by name.
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
/// Identity and root of the current run; use `Res<RunHandle>` when a system
/// needs the run id, the run root that holds the durable files, or whether the
/// run resumes an earlier one.
///
/// # Scope
///
/// **Global.** Every branch writes inside the same durable run.
///
/// # Provided by
///
/// Consumer-supplied. It has no constructor: the host fills in the public fields
/// with a struct literal and registers the value as a global, directly or
/// through [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`agent`](fn@super::agent) block reads it.
///
/// # Access
///
/// `Res<RunHandle>` only. The value is immutable: it is plain data with no
/// interior mutability, and nothing changes it after the host registers it.
///
/// # Alternatives
///
/// [`FlowSettings`](super::FlowSettings) holds what stays the same across runs,
/// while this holds what belongs to one run. Use an ordinary local resource for
/// state that only one branch needs.
///
/// # Example system
///
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RunHandle;
/// #[system]
/// async fn inspect(value:Res<RunHandle>)->std::path::PathBuf { value.root.clone() }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunHandle {
    /// Identifier of the run.
    pub run_id: String,
    /// Run root: the directory that holds every durable file of the run.
    pub root: PathBuf,
    /// Whether this run starts over or resumes an earlier one. An
    /// [`agent`](fn@super::agent) call restores a step the journal records as
    /// done only in a resumed run.
    pub mode: RunMode,
}
impl GlobalResource for RunHandle {}
/// The executor that runs one role call; use `Res<AgentRunner>` when a system
/// must start an isolated agent session for a call.
///
/// The [`agent`](fn@super::agent) block calls it once per call. It wraps any
/// [`TaskRunner`], so the same graph runs against a real model session or an
/// offline script.
///
/// # Scope
///
/// **Global.** Concurrent graph branches share one replaceable executor.
///
/// # Provided by
///
/// Consumer-supplied. The host wraps its own `Arc<dyn TaskRunner>` with
/// [`AgentRunner::new`] and registers the value as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource).
///
/// # Access
///
/// `Res<AgentRunner>` only. The wrapper is immutable and holds a shared
/// `Arc<dyn TaskRunner>`. [`TaskRunner::run`] takes `&self`, so any state a
/// runner keeps lives inside the runner, behind its own synchronization.
///
/// # Axis
///
/// The executor axis: how one role call runs, from an offline scripted session
/// to a live model session. This resource holds whichever point the host
/// chooses. Nothing is registered by default; the trivial point is
/// [`ScriptedRunner`](super::ScriptedRunner), which never calls a model.
///
/// # Alternatives
///
/// [`ScriptedRunner`](super::ScriptedRunner), loaded from a
/// [`Scenario`](super::Scenario), replaces a live runner in offline tests, so a
/// graph can be exercised without a model, a process or any spend. Any other
/// [`TaskRunner`] implementation can take its place the same way.
///
/// # Example system
///
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
