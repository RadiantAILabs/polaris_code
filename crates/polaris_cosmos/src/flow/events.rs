//! Event lines and shared phase status.
use super::{PhasePolicy, PhaseStatus, io::*};
use polaris::system::resource::GlobalResource;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};
/// Name of the event log file when the host does not choose one.
const DEFAULT_EVENT_FILE: &str = "events.jsonl";
/// Status note recorded for a phase that a user interruption ended.
const INTERRUPTED_NOTE: &str = "interrupted";
/// Global event sink; concurrent branches append whole, flushed lines under one lock.
///
/// # Scope
/// Global because all branches append to one ordered event stream.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<EventLog>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::EventLog;
/// #[system]
/// async fn inspect(value:Res<EventLog>)->bool { value.emit("checkpoint",None,serde_json::json!({})).is_ok() }
/// ```
#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
    run_id: String,
    lock: Arc<Mutex<()>>,
    heartbeats: Arc<HeartbeatTimers>,
}
#[derive(Debug, Default)]
struct HeartbeatTimers(Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>);
impl Drop for HeartbeatTimers {
    fn drop(&mut self) {
        let timers = self.0.get_mut().unwrap_or_else(PoisonError::into_inner);
        for task in timers.values() {
            task.abort();
        }
    }
}
impl GlobalResource for EventLog {}
impl EventLog {
    /// Creates a pipeline event sink that writes `events.jsonl` under `root`.
    ///
    /// Use [`with_file_name`](Self::with_file_name) to pick another file.
    pub fn open(root: &Path, run_id: &str) -> Self {
        Self {
            path: root.join(DEFAULT_EVENT_FILE),
            run_id: run_id.into(),
            lock: Arc::default(),
            heartbeats: Arc::default(),
        }
    }
    /// Writes events to `name` under the run root instead of the default file.
    ///
    /// Call this before the first phase starts: running heartbeat timers keep
    /// the path they were started with.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when `name` is not a safe relative path.
    pub fn with_file_name(mut self, name: &str) -> FlowResult<Self> {
        self.path = self.path.with_file_name(relative(name)?);
        Ok(self)
    }
    /// Starts sixty-second phase heartbeats; replacing a phase cancels its old timer.
    ///
    /// The timer ends with [`stop_phase`](Self::stop_phase),
    /// [`stop_all`](Self::stop_all), or when the last clone of the log drops.
    ///
    /// # Panics
    ///
    /// Panics when called outside a Tokio runtime, because the timer is a
    /// spawned task.
    pub fn start_phase(&self, phase: &str) {
        self.stop_phase(phase);
        let path = self.path.clone();
        let run_id = self.run_id.clone();
        let lock = self.lock.clone();
        let name = phase.to_owned();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let _lock = lock.guard();
                let _ = append_json(
                    &path,
                    &json!({"event":"heartbeat","fields":{"phase_name":name,"elapsed_seconds":started.elapsed().as_secs_f64()},"phase":name,"session_id":run_id,"source":"pipeline","timestamp":timestamp(),"worker_index":0}),
                );
            }
        });
        self.heartbeats.0.guard().insert(phase.into(), task);
    }
    /// Stops a phase timer; dropping the last sink also stops every timer.
    pub fn stop_phase(&self, phase: &str) {
        if let Some(task) = self.heartbeats.0.guard().remove(phase) {
            task.abort();
        }
    }
    /// Stops every running phase timer, for a host that is shutting down or
    /// abandoning phases that never reached their end.
    pub fn stop_all(&self) {
        let timers = std::mem::take(&mut *self.heartbeats.0.guard());
        for task in timers.into_values() {
            task.abort();
        }
    }
    /// Emits one event line with the standard envelope around `fields`.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the line cannot be written.
    pub fn emit(
        &self,
        event: &str,
        phase: Option<&str>,
        fields: serde_json::Value,
    ) -> FlowResult<()> {
        let _lock = self.lock.guard();
        append_json(
            &self.path,
            &json!({"event":event,"fields":fields,"phase":phase,"session_id":self.run_id,"source":"pipeline","timestamp":timestamp(),"worker_index":0}),
        )
    }
}
/// Why a workflow stopped starting new work.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
pub enum HaltReason {
    /// A user interruption. It outranks every failure policy.
    Interrupted,
    /// A phase with a fatal failure policy failed.
    Failed,
}
#[derive(Debug, Default)]
struct PhaseState {
    statuses: BTreeMap<String, PhaseStatus>,
    halt: Option<HaltReason>,
    incomplete: bool,
    started: BTreeMap<String, Instant>,
}

/// Global phase lifecycle state shared by phase checks and workflow edges.
///
/// # Scope
/// Global because phase checks and workflow edges share lifecycle status.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<PhaseControl>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::PhaseControl;
/// #[system]
/// async fn inspect(value:Res<PhaseControl>)->bool { value.halted() }
/// ```
#[derive(Clone, Debug)]
pub struct PhaseControl {
    root: PathBuf,
    enabled: Vec<String>,
    deadlines: BTreeMap<String, Duration>,
    inner: Arc<Mutex<PhaseState>>,
}
impl GlobalResource for PhaseControl {}
impl PhaseControl {
    /// Opens saved phase results with a fresh stop posture for a resumed run.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when saved phase results cannot be read.
    pub fn open(
        root: &Path,
        enabled: Vec<String>,
        deadlines: BTreeMap<String, Duration>,
    ) -> FlowResult<Self> {
        let path = root.join("pipeline_state.json");
        let statuses = if path.exists() {
            read_json(&path)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            root: root.into(),
            enabled,
            deadlines,
            inner: Arc::new(Mutex::new(PhaseState {
                statuses,
                ..PhaseState::default()
            })),
        })
    }
    /// Whether a phase was requested: only phases in the enabled list run.
    pub fn enabled(&self, name: &str) -> bool {
        self.enabled.iter().any(|p| p == name)
    }
    /// One-based position and total for phase progress events.
    pub fn progress(&self, name: &str) -> (usize, usize) {
        let total = self.enabled.len();
        self.enabled
            .iter()
            .position(|phase| phase == name)
            .map_or((total + 1, total + 1), |index| (index + 1, total))
    }
    /// Whether a fatal result or interruption prevents all later phases.
    pub fn halted(&self) -> bool {
        self.inner.guard().halt.is_some()
    }
    /// Terminal stop reason, if any.
    pub fn halt_reason(&self) -> Option<HaltReason> {
        self.inner.guard().halt
    }
    /// Whether a user interruption stopped the workflow.
    pub fn interrupted(&self) -> bool {
        self.halt_reason() == Some(HaltReason::Interrupted)
    }
    /// Whether an incomplete phase has been observed.
    pub fn incomplete(&self) -> bool {
        self.inner.guard().incomplete
    }
    /// Records a phase's typed result; errors never require throwing across parallel lanes.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the phase results cannot be saved.
    pub fn set_status(&self, name: &str, status: PhaseStatus) -> FlowResult<()> {
        let mut s = self.inner.guard();
        s.statuses.insert(name.into(), status);
        write_json(&self.root.join("pipeline_state.json"), &s.statuses)
    }
    /// Current status from a phase's check systems.
    pub fn status(&self, name: &str) -> Option<PhaseStatus> {
        self.inner.guard().statuses.get(name).cloned()
    }
    /// Snapshot used by final reporting.
    pub fn statuses(&self) -> BTreeMap<String, PhaseStatus> {
        self.inner.guard().statuses.clone()
    }
    /// Marks a user interruption, independent of optional-phase failure policy.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the phase results cannot be saved.
    pub fn interrupt(&self, name: &str) -> FlowResult<()> {
        self.inner.guard().halt = Some(HaltReason::Interrupted);
        self.set_status(name, PhaseStatus::Failed(INTERRUPTED_NOTE.into()))
    }
    /// Starts a phase timer and clears its previous interrupted status.
    pub fn start(&self, name: &str) {
        let mut s = self.inner.guard();
        s.statuses.remove(name);
        s.started.insert(name.into(), Instant::now());
    }
    /// Whether the configured phase deadline elapsed.
    pub fn expired(&self, name: &str) -> bool {
        let s = self.inner.guard();
        self.deadlines
            .get(name)
            .zip(s.started.get(name))
            .is_some_and(|(limit, start)| start.elapsed() >= *limit)
    }
    /// Time remaining in this phase, or none when no deadline was configured.
    pub fn remaining(&self, name: &str) -> Option<Duration> {
        let state = self.inner.guard();
        self.deadlines
            .get(name)
            .zip(state.started.get(name))
            .map(|(limit, start)| limit.saturating_sub(start.elapsed()))
    }
    /// Finalizes a status through the phase failure policy.
    ///
    /// An interruption overrides the recorded status and skips the policy.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the phase results cannot be saved.
    pub fn finish(&self, name: &str, policy: PhasePolicy) -> FlowResult<(PhaseStatus, f64)> {
        let mut s = self.inner.guard();
        let elapsed = s
            .started
            .get(name)
            .map_or(0.0, |start| start.elapsed().as_secs_f64());
        let recorded = s
            .statuses
            .get(name)
            .cloned()
            .unwrap_or(PhaseStatus::Completed);
        let status = if s.halt == Some(HaltReason::Interrupted) {
            PhaseStatus::Failed(INTERRUPTED_NOTE.into())
        } else {
            match recorded {
                PhaseStatus::Failed(note) => match policy {
                    PhasePolicy::Fatal => {
                        s.halt = Some(HaltReason::Failed);
                        PhaseStatus::Failed(note)
                    }
                    PhasePolicy::Degrade => PhaseStatus::Degraded(note),
                    PhasePolicy::Incomplete => {
                        s.incomplete = true;
                        PhaseStatus::Failed(note)
                    }
                },
                other => other,
            }
        };
        s.statuses.insert(name.into(), status.clone());
        write_json(&self.root.join("pipeline_state.json"), &s.statuses)?;
        Ok((status, elapsed))
    }
}
/// Stable event status and note projection.
pub fn phase_status(status: &PhaseStatus) -> (&'static str, &str) {
    match status {
        PhaseStatus::Completed => ("completed", ""),
        PhaseStatus::Degraded(s) => ("degraded", s),
        PhaseStatus::Failed(s) => ("failed", s),
        PhaseStatus::Skipped(s) => ("skipped", s),
    }
}

/// Global wall-clock boundary for starting new calls.
///
/// # Scope
/// Global because every role call shares the same wall-clock boundary.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<CallDeadline>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::CallDeadline;
/// #[system]
/// async fn inspect(value:Res<CallDeadline>)->bool { value.expired() }
/// ```
#[derive(Clone, Debug)]
pub struct CallDeadline {
    start: Instant,
    limit: Option<Duration>,
}
impl GlobalResource for CallDeadline {}
impl CallDeadline {
    /// Creates a deadline that starts now; `None` means no wall limit.
    pub fn new(limit: Option<Duration>) -> Self {
        Self {
            start: Instant::now(),
            limit,
        }
    }
    /// Creates a deadline with previously elapsed time, including an expired one.
    pub fn with_elapsed(limit: Duration, elapsed: Duration) -> Self {
        let now = Instant::now();
        Self {
            start: now.checked_sub(elapsed).unwrap_or(now),
            limit: Some(limit),
        }
    }
    /// Whether no new role call may begin.
    pub fn expired(&self) -> bool {
        self.limit
            .is_some_and(|limit| self.start.elapsed() >= limit)
    }
    /// Configured wall limit in seconds.
    pub fn limit_seconds(&self) -> Option<u64> {
        self.limit.map(|limit| limit.as_secs())
    }
    /// Elapsed seconds including time carried across a restart.
    pub fn elapsed_seconds(&self) -> u64 {
        self.start.elapsed().as_secs()
    }
}
