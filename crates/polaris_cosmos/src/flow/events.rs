//! Original-shaped event lines and shared phase status.
use super::{PhasePolicy, PhaseStatus, io::*};
use polaris::system::resource::GlobalResource;
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
/// Global event sink; concurrent branches append whole, flushed lines under one lock.
#[derive(Clone)]
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
pub struct EventLog {
    path: PathBuf,
    run_id: String,
    lock: Arc<Mutex<()>>,
    heartbeats: Arc<HeartbeatTimers>,
}
#[derive(Default)]
struct HeartbeatTimers(Mutex<BTreeMap<String, tokio::task::JoinHandle<()>>>);
impl Drop for HeartbeatTimers {
    fn drop(&mut self) {
        for task in self.0.get_mut().unwrap().values() {
            task.abort();
        }
    }
}
impl GlobalResource for EventLog {}
impl EventLog {
    /// Creates a pipeline event sink.
    pub fn open(root: &Path, run_id: &str) -> Self {
        Self {
            path: root.join("pipeline_events.jsonl"),
            run_id: run_id.into(),
            lock: Arc::default(),
            heartbeats: Arc::default(),
        }
    }
    /// Starts sixty-second phase heartbeats; replacing a phase cancels its old timer.
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
                let _lock = lock.lock().unwrap();
                let _ = append_json(
                    &path,
                    &json!({"event":"heartbeat","fields":{"phase_name":name,"elapsed_seconds":started.elapsed().as_secs_f64()},"phase":name,"session_id":run_id,"source":"pipeline","timestamp":timestamp(),"worker_index":0}),
                );
            }
        });
        self.heartbeats.0.lock().unwrap().insert(phase.into(), task);
    }
    /// Stops a phase timer; dropping the last sink also stops every timer.
    pub fn stop_phase(&self, phase: &str) {
        if let Some(task) = self.heartbeats.0.lock().unwrap().remove(phase) {
            task.abort();
        }
    }
    /// Emits one original-shaped line.
    pub fn emit(
        &self,
        event: &str,
        phase: Option<&str>,
        fields: serde_json::Value,
    ) -> FlowResult<()> {
        let _lock = self.lock.lock().unwrap();
        append_json(
            &self.path,
            &json!({"event":event,"fields":fields,"phase":phase,"session_id":self.run_id,"source":"pipeline","timestamp":timestamp(),"worker_index":0}),
        )
    }
}
#[derive(Default)]
struct PhaseState {
    statuses: BTreeMap<String, PhaseStatus>,
    halt: Option<String>,
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
#[derive(Clone)]
pub struct PhaseControl {
    root: PathBuf,
    enabled: Vec<String>,
    deadlines: BTreeMap<String, Duration>,
    inner: Arc<Mutex<PhaseState>>,
}
impl GlobalResource for PhaseControl {}
impl PhaseControl {
    /// Opens saved phase results with a fresh stop posture for a resumed run.
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
    /// Whether a phase was requested.
    pub fn enabled(&self, name: &str) -> bool {
        self.enabled.iter().any(|p| p == name) || name == "review" || name == "revision"
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
        self.inner.lock().unwrap().halt.is_some()
    }
    /// Terminal stop reason, if any.
    pub fn halt_reason(&self) -> Option<String> {
        self.inner.lock().unwrap().halt.clone()
    }
    /// Whether an incomplete phase has been observed.
    pub fn incomplete(&self) -> bool {
        self.inner.lock().unwrap().incomplete
    }
    /// Records a phase's typed result; errors never require throwing across parallel lanes.
    pub fn set_status(&self, name: &str, status: PhaseStatus) -> FlowResult<()> {
        let mut s = self.inner.lock().unwrap();
        s.statuses.insert(name.into(), status);
        write_json(&self.root.join("pipeline_state.json"), &s.statuses)
    }
    /// Current status from a phase's check systems.
    pub fn status(&self, name: &str) -> Option<PhaseStatus> {
        self.inner.lock().unwrap().statuses.get(name).cloned()
    }
    /// Snapshot used by final reporting.
    pub fn statuses(&self) -> BTreeMap<String, PhaseStatus> {
        self.inner.lock().unwrap().statuses.clone()
    }
    /// Marks a user interruption, independent of optional-phase failure policy.
    pub fn interrupt(&self, name: &str) -> FlowResult<()> {
        self.inner.lock().unwrap().halt = Some("interrupted".into());
        self.set_status(name, PhaseStatus::Failed("interrupted".into()))
    }
    /// Starts a phase timer and clears its previous interrupted status.
    pub fn start(&self, name: &str) {
        let mut s = self.inner.lock().unwrap();
        s.statuses.remove(name);
        s.started.insert(name.into(), Instant::now());
    }
    /// Whether the configured phase deadline elapsed.
    pub fn expired(&self, name: &str) -> bool {
        let s = self.inner.lock().unwrap();
        self.deadlines
            .get(name)
            .zip(s.started.get(name))
            .is_some_and(|(limit, start)| start.elapsed() >= *limit)
    }
    /// Time remaining in this phase, or none when no deadline was configured.
    pub fn remaining(&self, name: &str) -> Option<Duration> {
        let state = self.inner.lock().unwrap();
        self.deadlines
            .get(name)
            .zip(state.started.get(name))
            .map(|(limit, start)| limit.saturating_sub(start.elapsed()))
    }
    /// Finalizes a status through the phase failure policy.
    pub fn finish(&self, name: &str, policy: PhasePolicy) -> FlowResult<(PhaseStatus, f64)> {
        let mut s = self.inner.lock().unwrap();
        let elapsed = s
            .started
            .get(name)
            .map_or(0.0, |start| start.elapsed().as_secs_f64());
        let status = s
            .statuses
            .get(name)
            .cloned()
            .unwrap_or(PhaseStatus::Completed);
        let status = if s.halt.as_deref() == Some("interrupted") {
            PhaseStatus::Failed("interrupted".into())
        } else {
            status
        };
        let status = match status {
            PhaseStatus::Failed(note) if note != "interrupted" => match policy {
                PhasePolicy::Fatal => {
                    s.halt = Some("failed".into());
                    PhaseStatus::Failed(note)
                }
                PhasePolicy::Degrade => PhaseStatus::Degraded(note),
                PhasePolicy::Incomplete => {
                    s.incomplete = true;
                    PhaseStatus::Failed(note)
                }
            },
            other => other,
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
#[derive(Clone)]
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
pub struct CallDeadline {
    start: Instant,
    limit: Option<Duration>,
}
impl GlobalResource for CallDeadline {}
impl CallDeadline {
    /// Creates an optional deadline; zero means no wall limit.
    pub fn new(seconds: u64) -> Self {
        Self {
            start: Instant::now(),
            limit: (seconds > 0).then(|| Duration::from_secs(seconds)),
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
