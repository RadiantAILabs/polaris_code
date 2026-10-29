//! Managed shell runs pushed out of a turn — by the user's `Ctrl-T` chord or by
//! the model's own `run_in_background` call — the four `/commands` that list,
//! inspect, follow and end them, and the retained results both read back.
//!
//! # What a background task is here
//!
//! A **process**, not an agent. [`ShellExecutor`] owns every managed run: its
//! `ProcessTable` and the supervisor task that holds the child belong to the
//! server, not to the graph run that started it. Two rules of that ownership
//! decide the shape of backgrounding:
//!
//! - a run started by `execute` — which is what `run_command` calls — is
//!   **reserved** for that caller. It is listed and stoppable, but
//!   [`ShellExecutor::wait_for`] returns `None` for it rather than take the
//!   result out from under the caller; and
//! - the caller's `ShellProcess` handle **requests a stop when it is dropped**,
//!   so a wait that is simply dropped ends the child.
//!
//! So backgrounding is the host **re-homing the wait**, not abandoning it. The
//! tool runs in a task of its own ([`ToolRun`]); the turn stops waiting on that
//! task and the reporter waits on it instead. Nothing is re-run, nothing spawns
//! twice, and the reservation is never bypassed: the result reaches the host
//! through the one caller the executor reserved it for, as the same `executed`
//! response the model would have received.
//!
//! This module is the host's side of that: the table that remembers which
//! managed run belongs to which tool block and carries its wait once
//! backgrounded, the [`ShellSink`] that folds the executor's events into a
//! bounded tail, and the commands that read it back. It mints no identity. A
//! task is named by the [`ShellProcessId`] the executor allocated, rendered as
//! the same decimal the model has already seen in `ExecutionResult::process`.
//!
//! # What it deliberately does not do
//!
//! | Not here | Because |
//! |---|---|
//! | Kill, spawn or re-run anything | Backgrounding calls no `start`, `start_confirmed` or `execute`. `/stop` is [`ShellExecutor::stop`], the per-process stop `ShellExecutor` already provides. |
//! | Take a result from the executor | [`ShellExecutor::wait_for`] refuses a reserved run, and every `run_command` run is reserved. Starting runs unreserved instead would let *any* holder of the executor race the tool for a foreground result. The reporter waits on the invocation's own task — the caller the result belongs to. |
//! | Extend a running command's deadline | The deadline is fixed at start upstream; moving it would need a change in Polaris (`polaris-ai`). Everything here *names* the deadline instead of hiding it. |
//! | Write the model's history from outside the graph | A completion reaches [`ContextManager`](crate::agents::ContextManager) only through the mid-turn [`TurnSteering`] boundary. When there is no turn to steer, the completion is reconciled into the transcript **and says so** — and the terminal result is retained here, so the model can read it back on its next turn. |
//! | Start a turn to report a completion | Steering degrades to a queued prompt with no active run, which would spend the user's tokens on a call they never asked for. [`TurnSteering::steer_mid_turn`] queues nothing instead. |
//! | Hold a task list in `polaris-tui` | The panel learns a count and a callback. The list is this table joined with the executor's own. |
//!
//! # Read-back after the run ends
//!
//! A terminal result is observable exactly once — [`ShellExecutor::wait_for`]
//! consumes it, a reserved run's result reaches only its own caller — and
//! [`ShellProcessStatus`] carries no exit code, so a completion nobody kept is
//! a completion nobody can be told about. Whichever waiter this host runs
//! ([`BackgroundReporter::watch`] interactively, [`collect_result`] for a front
//! end with no reporter) therefore hands what it observed straight to
//! [`BackgroundTasks::finish`] as a [`CommandResult`], which moves the task off
//! the live list into a bounded ring of [`FinishedTask`] records. That ring is
//! what `task_output` reads, and it is why a command that finished while the
//! session was idle is still answerable on the next turn.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};

use serde::Deserialize;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tracing::Instrument as _;

use polaris::plugins::{IO_TYPE, IOMessage};
use polaris::sessions::SessionId;
use polaris::shell::{
    ExecutionResult, ProcessTreeGuarantee, ShellEvent, ShellEventDelivery, ShellExecutor,
    ShellOutcome, ShellProcessId, ShellProcessState, ShellProcessStatus, ShellSink,
};
use polaris::system::resource::LocalResource;
use polaris::tools::{ToolContext, ToolError, ToolRegistry};
use polaris_tui::commands::CommandSpec;

use super::transcript::{PHASE_ERROR, PHASE_OK, PHASE_RUNNING, tool_message};
use super::{
    CommandError, CommandOutcome, CommandRegistry, SteerRefused, TerminalOutput, TurnSteering,
};

/// How often a running command's tail is forwarded to the panel.
///
/// Measured against the consumer, not guessed: the shell repaints from
/// `tokio::time::interval(TuiConfig::tick_rate)` and that default is 100 ms
/// (`polaris-tui`'s `config.rs`), which `polaris-code` does not override. An
/// update per chunk would therefore be discarded by the renderer, and anything
/// slower than the repaint shows as visible lag. One update per repaint is the
/// most the user can actually see, so that is what the host sends — **one
/// [`IOMessage`] per tick at most, never one per chunk**.
pub const COALESCE_TICK: Duration = Duration::from_millis(100);

/// Lines of a command's output the host keeps for `/task` and `/fg`.
const TAIL_LINES: usize = 200;

/// Longest single retained line. The executor's own capture, and its complete
/// overflow file, remain the authority for the full output.
const TAIL_LINE_BYTES: usize = 2 * 1024;

/// Finished runs kept for read-back, newest last.
///
/// Each record holds the executor's own [`ExecutionResult`], whose capture is
/// already bounded by `ShellConfig::max_output_bytes`, so this ring is bounded
/// by that product and not by how much a command printed.
pub const FINISHED_RETAINED: usize = 16;

/// The deadline a model-started background run gets when it names none.
///
/// Deliberately far longer than the foreground default. A foreground command
/// holds the turn, so its deadline bounds how long the model is stuck waiting;
/// a backgrounded one blocks nothing, so the only thing left for a deadline to
/// buy is a guarantee that a forgotten task cannot outlive interest in it. One
/// hour is long enough for a cold `cargo test` or a release build and short
/// enough that "still running" always means something. It is a ceiling, not a
/// target: the model can name a shorter `timeout_secs`, and there is no way to
/// ask for an unbounded one, because a task the harness has stopped tracking
/// while the operating system still runs it is the outcome this whole surface
/// exists to prevent.
pub const BACKGROUND_DEFAULT_TIMEOUT_SECS: u64 = 3600;

/// The user-visible handle for a managed run.
///
/// [`ShellProcessId`] is opaque by construction, but it is `Serialize` and is
/// already carried to the model as `ExecutionResult::process`, so its
/// serialized form is the identity both the user and the model have seen. This
/// reads it back rather than minting a second numbering that could disagree.
#[must_use]
pub fn task_number(id: ShellProcessId) -> u64 {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| value.as_u64())
        .unwrap_or_default()
}

/// Why a backgrounding request was refused.
///
/// Every variant is explainable in the user's terms, and every one of them is
/// shown: a chord that quietly does nothing is the failure this type exists to
/// rule out. The single case with no refusal is a host that installed no
/// handle at all, where the chord is simply unbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundRefusal {
    /// No tool invocation is in flight.
    NothingRunning,
    /// The block is still waiting on the approval dialog.
    AwaitingApproval {
        /// The tool that asked.
        label: String,
    },
    /// The invocation is not a shell command.
    NotBackgroundable {
        /// The tool that is running.
        label: String,
    },
    /// The command was accepted but has not been given a run identity yet.
    NotStarted {
        /// The tool that is running.
        label: String,
    },
}

impl BackgroundRefusal {
    /// The sentence shown to the user. Says what happened and what to do.
    #[must_use]
    pub fn notice(&self) -> String {
        match self {
            Self::NothingRunning => {
                "nothing is running to background — Ctrl-T applies to a command the agent is \
                 waiting on"
                    .to_string()
            }
            Self::AwaitingApproval { label } => format!(
                "answer the approval first — an unapproved `{label}` cannot be backgrounded"
            ),
            Self::NotBackgroundable { label } => {
                format!("only shell commands can be backgrounded; `{label}` has to finish")
            }
            Self::NotStarted { label } => {
                format!("`{label}` has not started yet — try again once it is running")
            }
        }
    }

    /// The refusal as the `error` block the panel always renders.
    #[must_use]
    pub fn into_message(self) -> IOMessage {
        IOMessage::system_text(self.notice()).with_metadata(IO_TYPE, "error")
    }
}

/// What the host is currently executing, as far as backgrounding is concerned.
#[derive(Debug, Clone)]
enum Active {
    /// Nothing is in flight.
    Idle,
    /// A tool block is open and waiting on `UserIO::confirm`.
    AwaitingApproval { call_id: String, label: String },
    /// A tool block is open and the tool is executing.
    Executing { call_id: String, label: String },
}

/// One managed run the host is tracking.
#[derive(Debug)]
struct Task {
    /// Executor-local identity — the control handle `/stop` takes.
    id: ShellProcessId,
    /// The tool block this run keeps for its whole life.
    call_id: String,
    /// The `message` block the invocation was nested under, if any.
    parent: Option<String>,
    /// Short tool name, for the block's label.
    label: String,
    /// The command as the caller wrote it.
    command: String,
    /// Which session started it, so a list can say when it is not this one.
    session: SessionId,
    /// Which turn of that session started it.
    turn: u32,
    /// When the host observed the run start.
    started: Instant,
    /// The deadline the executor fixed at start. It cannot be moved.
    deadline: Duration,
    /// Last whole lines of output, bounded.
    tail: VecDeque<String>,
    /// Bytes read since the last newline.
    partial: String,
    /// Events the sink truthfully reported dropping.
    dropped: u64,
    /// Whether in-memory capture hit its per-stream limit.
    capture_overflowed: bool,
    /// Whether the user pushed this run out of its turn.
    backgrounded: bool,
    /// Whether the user's attention is following it.
    followed: bool,
    /// Bumped by every chunk; compared against `emitted` to coalesce.
    revision: u64,
    /// The revision the host last forwarded to the panel.
    emitted: u64,
    /// The invocation's wait, held here from the moment the turn stopped
    /// waiting on it until the reporter takes it. `None` while the turn is
    /// still waiting, and again once a watcher has it.
    run: Option<ToolRun>,
}

impl Task {
    fn remaining(&self) -> Duration {
        self.deadline.saturating_sub(self.started.elapsed())
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        self.partial.push_str(&String::from_utf8_lossy(bytes));
        while let Some(index) = self.partial.find('\n') {
            let mut line: String = self.partial.drain(..=index).collect();
            while line.ends_with(['\n', '\r']) {
                line.pop();
            }
            self.push_line(line);
        }
        // A command that prints a progress line without a newline would
        // otherwise grow `partial` without bound.
        if self.partial.len() > TAIL_LINE_BYTES {
            let keep = self.partial.floor_char_boundary(TAIL_LINE_BYTES);
            self.partial.drain(..self.partial.len() - keep);
        }
        self.revision = self.revision.saturating_add(1);
    }

    fn push_line(&mut self, mut line: String) {
        if line.len() > TAIL_LINE_BYTES {
            line.truncate(line.floor_char_boundary(TAIL_LINE_BYTES));
        }
        if line.trim().is_empty() {
            return;
        }
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    /// The newest whole line, or the partial one when nothing has ended yet.
    fn last_line(&self) -> Option<&str> {
        self.tail
            .back()
            .map(String::as_str)
            .or(Some(self.partial.trim_end()).filter(|text| !text.is_empty()))
    }
}

/// One tool invocation, running in a task the turn does not own outright.
///
/// The turn awaits it, and backgrounding hands the same wait to the host
/// instead of dropping it. Dropping is not harmless: `run_command` calls
/// `ShellExecutor::execute`, whose wait holds the run's `ShellProcess` handle,
/// and that handle requests a stop when dropped — so a dropped wait ends the
/// child. The result is also reserved for that caller, so nobody could collect
/// it afterwards either. Keeping the caller alive in its own task keeps both:
/// the child runs on, and its result comes back through the caller.
///
/// The task is the same [`ToolRegistry::to_arc`] + `Tool::execute` pair that
/// `ToolRegistry::execute_with` performs inline, given the run's own copy of
/// the [`ToolContext`] so nothing borrows the system that started it.
#[derive(Debug)]
pub struct ToolRun {
    handle: JoinHandle<Result<serde_json::Value, ToolError>>,
}

impl ToolRun {
    /// Start `name` from `registry` with `args` and `context` in its own task.
    ///
    /// An unknown tool fails inside the task, exactly as
    /// `ToolRegistry::execute_with` would have, so the caller sees one shape
    /// of outcome.
    #[must_use]
    pub fn spawn(
        registry: &ToolRegistry,
        name: &str,
        args: serde_json::Value,
        context: ToolContext,
    ) -> Self {
        let tool = registry.to_arc(name);
        let name = name.to_string();
        let handle = tokio::spawn(
            async move {
                let tool = tool.ok_or_else(|| ToolError::unknown_tool(name))?;
                tool.execute(args, &context).await
            }
            // Keep the tool's own tracing events under the node span that
            // started it, as an inline call would have.
            .instrument(tracing::Span::current()),
        );
        Self { handle }
    }

    /// End the task, dropping the tool's future.
    ///
    /// This is what an interrupted invocation has always done to its wait; it
    /// is *not* used on a backgrounded one, whose wait is handed over instead.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

/// The wait itself. The outer `Err` is a task that ended without an answer —
/// aborted, or a tool that panicked — and the inner one is the tool's own.
///
/// As with the inline future this replaces, it must not be polled again once
/// it has completed: both waiters here `break` out of their `select!` on the
/// first ready poll.
impl Future for ToolRun {
    type Output = Result<Result<serde_json::Value, ToolError>, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.handle).poll(cx)
    }
}

#[derive(Debug)]
struct Inner {
    active: Option<Active>,
    /// Backgrounding was requested for this call id and not yet consumed.
    requested: Option<String>,
    tasks: Vec<Task>,
    /// Runs that reached a terminal state, oldest first, bounded by
    /// [`FINISHED_RETAINED`]. Kept out of `tasks` so the four `/commands` and
    /// the panel's running count read only the live list.
    finished: VecDeque<FinishedTask>,
    /// The executor's own default, so a run that named no `timeout_secs` still
    /// reports the deadline it actually has.
    default_timeout: Duration,
    /// Where a newly backgrounded task is announced. `None` for a host that
    /// wired no reporter: the run still detaches, and nothing waits on it.
    backgrounded: Option<mpsc::UnboundedSender<u64>>,
}

/// The host's table of managed runs, addressed by run identity.
///
/// Cheap to clone and shared by every holder — the sink that fills it, the
/// system that reads it, and the commands that report it all observe the same
/// state. It owns no server, no session, no IO channel and no executor: it is
/// a map plus the one flag a chord sets, in the shape [`TurnSteering`] already
/// established for host-owned turn state.
#[derive(Clone, Debug)]
pub struct BackgroundTasks {
    inner: Arc<Mutex<Inner>>,
}

impl LocalResource for BackgroundTasks {}

/// A snapshot of one tracked run, for rendering.
#[derive(Debug, Clone)]
pub struct TaskView {
    /// The user-visible handle.
    pub number: u64,
    /// Executor-local identity.
    pub id: ShellProcessId,
    /// The tool block it keeps.
    pub call_id: String,
    /// The `message` block it is nested under.
    pub parent: Option<String>,
    /// Short tool name.
    pub label: String,
    /// The command as written.
    pub command: String,
    /// The session that started it.
    pub session: SessionId,
    /// The turn of that session.
    pub turn: u32,
    /// Wall time since the host saw it start.
    pub elapsed: Duration,
    /// Time left before the executor's fixed deadline.
    pub remaining: Duration,
    /// The deadline itself.
    pub deadline: Duration,
    /// Newest output line the host has seen.
    pub last_line: Option<String>,
    /// Retained tail, oldest first.
    pub tail: Vec<String>,
    /// Events the sink reported dropping.
    pub dropped: u64,
    /// Whether in-memory capture hit its limit.
    pub capture_overflowed: bool,
    /// Whether the user pushed it out of its turn.
    pub backgrounded: bool,
}

/// One run that reached a terminal state, kept so it can still be read back.
///
/// The live [`TaskView`] is the snapshot taken as the run ended — its `tail`,
/// `dropped` and `capture_overflowed` fields are the host's own bounded view.
/// `result` is the run's own terminal answer: the exit code, the full retained
/// capture, the truncation flags and the overflow paths.
#[derive(Debug, Clone)]
pub struct FinishedTask {
    /// The task as it stood when it ended.
    pub task: TaskView,
    /// The run's terminal result, when this host observed it.
    ///
    /// A [`CommandResult`] either way: a reserved `run_command` run reaches the
    /// host as the tool's own `executed` response, a model-started one as the
    /// executor's `ExecutionResult`, and read-back must not be able to tell
    /// which route a task took.
    pub result: Option<CommandResult>,
    /// Why no result was observed, when `result` is `None`.
    ///
    /// A run whose result another waiter consumed, or that the executor failed,
    /// still gets a record: "the task is gone" and "the task never existed" are
    /// different answers and a read-back that conflated them would be lying.
    pub note: Option<String>,
}

/// The stream of runs the user pushed out of a turn.
///
/// A host takes this at construction and drives one
/// [`BackgroundReporter::watch`] per item. Dropping it is a valid choice —
/// a front end with no transcript to report into detaches runs just the same,
/// and simply never waits on them.
#[derive(Debug)]
pub struct Backgrounded(mpsc::UnboundedReceiver<u64>);

impl Backgrounded {
    /// The next run the user backgrounded, or `None` once the table is gone.
    pub async fn recv(&mut self) -> Option<u64> {
        self.0.recv().await
    }
}

impl BackgroundTasks {
    /// An empty table, and the stream of runs backgrounded into it.
    ///
    /// `default_timeout` is the executor's configured default, used to report
    /// the deadline of a run that named no `timeout_secs` of its own. The
    /// deadline is fixed when the run starts and cannot be moved, so every
    /// surface naming a task names what is left of it.
    #[must_use]
    pub fn new(default_timeout: Duration) -> (Self, Backgrounded) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let tasks = Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                requested: None,
                tasks: Vec::new(),
                finished: VecDeque::new(),
                default_timeout,
                backgrounded: Some(sender),
            })),
        };
        (tasks, Backgrounded(receiver))
    }

    /// A table nothing reports from, for a front end that wires no reporter.
    #[must_use]
    pub fn detached(default_timeout: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                active: None,
                requested: None,
                tasks: Vec::new(),
                finished: VecDeque::new(),
                default_timeout,
                backgrounded: None,
            })),
        }
    }

    /// The deadline a `run_command` invocation will actually get.
    ///
    /// Read from the call's own `timeout_secs` when it named one, and from the
    /// executor's default otherwise — the same resolution `start_inner` makes
    /// upstream, done here so the receipt can state it before the run begins.
    #[must_use]
    pub fn deadline_for(&self, arguments: &serde_json::Value) -> Duration {
        arguments
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(|| self.lock().default_timeout, Duration::from_secs)
    }

    /// The deadline a model-started background run will actually get.
    ///
    /// Same resolution as [`Self::deadline_for`] over the same `timeout_secs`
    /// argument, but falling back to [`BACKGROUND_DEFAULT_TIMEOUT_SECS`]
    /// instead of the executor's foreground default — a command that blocks
    /// nobody should not inherit the deadline that exists to stop a turn
    /// hanging.
    #[must_use]
    pub fn background_deadline_for(&self, arguments: &serde_json::Value) -> Duration {
        arguments
            .get("timeout_secs")
            .and_then(serde_json::Value::as_u64)
            .map_or_else(
                || Duration::from_secs(BACKGROUND_DEFAULT_TIMEOUT_SECS),
                Duration::from_secs,
            )
    }

    /// Announce a newly backgrounded run to whatever the host wired.
    ///
    /// Returns whether an announcement was actually made. A caller that starts
    /// a run nobody will wait on uses this to decide whether it has to collect
    /// the terminal result itself ([`collect_result`]): exactly one waiter per
    /// run, because [`ShellExecutor::wait_for`] consumes the result.
    pub fn report_backgrounded(&self, number: u64) -> bool {
        if let Some(sender) = &self.lock().backgrounded {
            // A closed channel means the host stopped listening; the run is
            // still detached and still discoverable through `/tasks`.
            return sender.send(number).is_ok();
        }
        false
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    /// Announce the invocation the host is about to run.
    ///
    /// The returned guard clears the slot however the invocation ends, so no
    /// exit path can leave the host believing something is still in flight.
    #[must_use]
    pub fn enter(&self, call_id: &str, label: &str, parent: Option<&str>) -> Invocation {
        self.lock().active = Some(Active::Executing {
            call_id: call_id.to_string(),
            label: label.to_string(),
        });
        Invocation {
            tasks: self.clone(),
            call_id: call_id.to_string(),
            label: label.to_string(),
            parent: parent.map(str::to_string),
        }
    }

    /// Request that the in-flight invocation be backgrounded.
    ///
    /// Answers immediately with a typed refusal when it cannot be honoured.
    /// Success only latches the request: the receipt is emitted by the system
    /// that owns the tool block, because that is the only place that can close
    /// the invocation exactly once.
    ///
    /// # Errors
    ///
    /// Returns the [`BackgroundRefusal`] describing what is in the way.
    pub fn request(&self) -> Result<(), BackgroundRefusal> {
        let mut inner = self.lock();
        let (call_id, label) = match inner.active.clone() {
            None | Some(Active::Idle) => return Err(BackgroundRefusal::NothingRunning),
            Some(Active::AwaitingApproval { label, .. }) => {
                return Err(BackgroundRefusal::AwaitingApproval { label });
            }
            Some(Active::Executing { call_id, label }) => (call_id, label),
        };
        if label != "run_command" {
            return Err(BackgroundRefusal::NotBackgroundable { label });
        }
        if !inner.tasks.iter().any(|task| task.call_id == call_id) {
            return Err(BackgroundRefusal::NotStarted { label });
        }
        inner.requested = Some(call_id);
        Ok(())
    }

    /// Whether backgrounding is pending for `call_id`.
    #[must_use]
    pub fn requested(&self, call_id: &str) -> bool {
        self.lock().requested.as_deref() == Some(call_id)
    }

    /// Take the pending request for `call_id`, if there is one.
    fn take_request(&self, call_id: &str) -> bool {
        let mut inner = self.lock();
        if inner.requested.as_deref() == Some(call_id) {
            inner.requested = None;
            true
        } else {
            false
        }
    }

    /// The coalesced update for `call_id`, if its output moved since the last.
    ///
    /// Returns `None` when nothing new arrived, which is what keeps a chatty
    /// command to one message per tick rather than one per chunk.
    #[must_use]
    pub fn take_update(&self, call_id: &str) -> Option<String> {
        let mut inner = self.lock();
        let task = inner.tasks.iter_mut().find(|t| t.call_id == call_id)?;
        if task.revision == task.emitted {
            return None;
        }
        task.emitted = task.revision;
        task.last_line().map(str::to_string)
    }

    /// A snapshot of the run behind `call_id`.
    #[must_use]
    pub fn by_call(&self, call_id: &str) -> Option<TaskView> {
        self.lock()
            .tasks
            .iter()
            .find(|task| task.call_id == call_id)
            .map(view)
    }

    /// A snapshot of the run the user knows as `number`.
    #[must_use]
    pub fn by_number(&self, number: u64) -> Option<TaskView> {
        self.lock()
            .tasks
            .iter()
            .find(|task| task_number(task.id) == number)
            .map(view)
    }

    /// Every tracked run, oldest identity first.
    #[must_use]
    pub fn all(&self) -> Vec<TaskView> {
        let mut views: Vec<_> = self.lock().tasks.iter().map(view).collect();
        views.sort_by_key(|task| task.number);
        views
    }

    /// Every retained finished run, oldest first.
    #[must_use]
    pub fn finished(&self) -> Vec<FinishedTask> {
        self.lock().finished.iter().cloned().collect()
    }

    /// The retained finished run the model knows as `number`.
    #[must_use]
    pub fn finished_by_number(&self, number: u64) -> Option<FinishedTask> {
        self.lock()
            .finished
            .iter()
            .find(|record| record.task.number == number)
            .cloned()
    }

    /// Mark a run as backgrounded and take custody of its wait, returning the
    /// snapshot to report.
    ///
    /// Gives the wait back when no run is tracked for `call_id`, so the caller
    /// still owns it and can decide what a wait nobody will report deserves.
    fn background(&self, call_id: &str, run: ToolRun) -> Result<TaskView, ToolRun> {
        let mut inner = self.lock();
        let Some(task) = inner.tasks.iter_mut().find(|t| t.call_id == call_id) else {
            return Err(run);
        };
        task.backgrounded = true;
        task.followed = false;
        task.run = Some(run);
        Ok(view(task))
    }

    /// Take the wait a backgrounded run carries, for the one watcher that will
    /// see it to its end. `None` when the task is unknown or its wait has
    /// already been taken.
    fn take_run(&self, number: u64) -> Option<ToolRun> {
        self.lock()
            .tasks
            .iter_mut()
            .find(|task| task_number(task.id) == number)
            .and_then(|task| task.run.take())
    }

    /// Mark a run backgrounded by its run identity rather than its tool block.
    ///
    /// The model's route: `run_in_background` holds the [`ShellProcessId`] the
    /// executor just handed it, not the call id of the invocation it is running
    /// inside. Same state change as [`Self::background`], and the same
    /// unfollowing — the user never asked to watch this one, so only its
    /// completion reaches the transcript unless `/fg` asks for more.
    #[must_use]
    pub fn background_process(&self, id: ShellProcessId) -> Option<TaskView> {
        let mut inner = self.lock();
        let task = inner.tasks.iter_mut().find(|task| task.id == id)?;
        task.backgrounded = true;
        task.followed = false;
        Some(view(task))
    }

    /// Follow or unfollow a run's output.
    fn set_followed(&self, number: u64, followed: bool) -> bool {
        let mut inner = self.lock();
        let Some(task) = inner
            .tasks
            .iter_mut()
            .find(|task| task_number(task.id) == number)
        else {
            return false;
        };
        task.followed = followed;
        // Re-follow shows the tail it has now, not only what arrives next.
        task.emitted = task.revision.saturating_sub(1);
        true
    }

    #[must_use]
    fn is_followed(&self, number: u64) -> bool {
        self.lock()
            .tasks
            .iter()
            .any(|task| task_number(task.id) == number && task.followed)
    }

    /// Retire a run off the live list, keeping what it produced.
    ///
    /// Called by the one waiter that consumed the run's terminal result. It
    /// removes the run from `/tasks`, `/task`, `/fg`, `/stop`, and the panel's
    /// running count, then retains the result for read-back.
    fn finish(&self, id: ShellProcessId, result: Option<CommandResult>, note: Option<String>) {
        let mut inner = self.lock();
        let Some(index) = inner.tasks.iter().position(|task| task.id == id) else {
            return;
        };
        let task = inner.tasks.remove(index);
        let record = FinishedTask {
            task: view(&task),
            result,
            note,
        };
        if inner.finished.len() == FINISHED_RETAINED {
            inner.finished.pop_front();
        }
        inner.finished.push_back(record);
    }

    /// How many tracked runs are still going, for the panel's meta row.
    #[must_use]
    pub fn running(&self) -> u64 {
        self.lock().tasks.len() as u64
    }
}

fn view(task: &Task) -> TaskView {
    TaskView {
        number: task_number(task.id),
        id: task.id,
        call_id: task.call_id.clone(),
        parent: task.parent.clone(),
        label: task.label.clone(),
        command: task.command.clone(),
        session: task.session.clone(),
        turn: task.turn,
        elapsed: task.started.elapsed(),
        remaining: task.remaining(),
        deadline: task.deadline,
        last_line: task.last_line().map(str::to_string),
        tail: task.tail.iter().cloned().collect(),
        dropped: task.dropped,
        capture_overflowed: task.capture_overflowed,
        backgrounded: task.backgrounded,
    }
}

/// The in-flight invocation's claim on the backgrounding slot.
///
/// Dropping it releases the claim, so a denial, an interruption, an error and
/// a normal return all leave the host truthful about what is running without
/// each exit path having to remember.
#[derive(Debug)]
pub struct Invocation {
    tasks: BackgroundTasks,
    call_id: String,
    label: String,
    parent: Option<String>,
}

impl Invocation {
    /// Report that the block is parked on the approval dialog.
    pub fn awaiting_approval(&self) {
        self.tasks.lock().active = Some(Active::AwaitingApproval {
            call_id: self.call_id.clone(),
            label: self.label.clone(),
        });
    }

    /// Report that the user approved and the tool is executing.
    pub fn executing(&self) {
        self.tasks.lock().active = Some(Active::Executing {
            call_id: self.call_id.clone(),
            label: self.label.clone(),
        });
    }

    /// The sink this invocation streams its managed run through.
    #[must_use]
    pub fn sink(&self, session: &SessionId, turn: u32, deadline: Duration) -> TaskSink {
        TaskSink {
            tasks: self.tasks.clone(),
            call_id: self.call_id.clone(),
            parent: self.parent.clone(),
            label: self.label.clone(),
            session: session.clone(),
            turn,
            deadline,
        }
    }

    /// Take the backgrounding request, if the user made one.
    #[must_use]
    pub fn take_request(&self) -> bool {
        self.tasks.take_request(&self.call_id)
    }

    /// Mark this invocation's run backgrounded, hand its wait to the host, and
    /// describe it.
    ///
    /// # Errors
    ///
    /// Returns the wait unchanged when the host tracks no run for this
    /// invocation, so nothing is dropped behind the caller's back.
    pub fn background(&self, run: ToolRun) -> Result<TaskView, ToolRun> {
        self.tasks.background(&self.call_id, run)
    }
}

impl Drop for Invocation {
    fn drop(&mut self) {
        let mut inner = self.tasks.lock();
        let still_ours = match &inner.active {
            Some(Active::AwaitingApproval { call_id, .. } | Active::Executing { call_id, .. }) => {
                *call_id == self.call_id
            }
            None | Some(Active::Idle) => false,
        };
        if still_ours {
            inner.active = Some(Active::Idle);
        }
        if inner.requested.as_deref() == Some(self.call_id.as_str()) {
            inner.requested = None;
        }
    }
}

/// The host's [`ShellSink`]: folds one managed run's events into the table.
///
/// Non-blocking by contract, so it appends to a bounded ring and returns
/// rather than sending anything. Nothing is silently lost: the executor's
/// capture and its complete overflow file remain the authority, and the two
/// ways this view can be partial — dropped events and a capture limit — are
/// both recorded and both reported by every surface that shows the tail.
#[derive(Debug, Clone)]
pub struct TaskSink {
    tasks: BackgroundTasks,
    call_id: String,
    parent: Option<String>,
    label: String,
    session: SessionId,
    turn: u32,
    deadline: Duration,
}

impl ShellSink for TaskSink {
    fn on_event(&self, event: ShellEvent) -> ShellEventDelivery {
        let mut inner = self.tasks.lock();
        match event {
            ShellEvent::Started {
                process, command, ..
            } => {
                inner.tasks.retain(|task| task.id != process);
                inner.tasks.push(Task {
                    id: process,
                    call_id: self.call_id.clone(),
                    parent: self.parent.clone(),
                    label: self.label.clone(),
                    command,
                    session: self.session.clone(),
                    turn: self.turn,
                    started: Instant::now(),
                    deadline: self.deadline,
                    tail: VecDeque::new(),
                    partial: String::new(),
                    dropped: 0,
                    capture_overflowed: false,
                    backgrounded: false,
                    followed: true,
                    revision: 0,
                    emitted: 0,
                    run: None,
                });
            }
            // Which pipe a chunk came from is deliberately not kept: the user
            // reads one interleaved view, exactly as a terminal would show it.
            ShellEvent::Chunk { process, bytes, .. } => {
                if let Some(task) = inner.tasks.iter_mut().find(|task| task.id == process) {
                    task.push_bytes(&bytes);
                }
            }
            // Both limits mean the same thing to this view: what the user can
            // still read is no longer the whole stream. The in-memory capture
            // stopping and the overflow file stopping differ only in how much
            // was kept, which the lossiness note already refuses to promise.
            ShellEvent::CaptureLimitReached { process, stream }
            | ShellEvent::OverflowLimitReached { process, stream } => {
                if let Some(task) = inner.tasks.iter_mut().find(|task| task.id == process) {
                    task.capture_overflowed = true;
                    let _ = stream;
                }
            }
            ShellEvent::Finished { process, .. } => {
                if let Some(task) = inner.tasks.iter_mut().find(|task| task.id == process) {
                    // Flush a last line the command never terminated, so the
                    // final tail is not missing the row the user cares most about.
                    let partial = std::mem::take(&mut task.partial);
                    task.push_line(partial);
                    task.revision = task.revision.saturating_add(1);
                }
            }
            // `ShellEvent` is `#[non_exhaustive]`: an event this sink has no
            // view for changes nothing it renders. The task view is the
            // in-memory tail, and both ways that tail can be partial —
            // dropped events and the capture limit — already have arms above.
            _ => {}
        }
        ShellEventDelivery::Accepted
    }
}

/// Render the remaining-time clause every surface naming a running task owes.
///
/// The deadline is fixed when the run starts and there is no way to move it,
/// so a surface that named a task without naming its deadline would be
/// promising time the runtime cannot give.
#[must_use]
pub fn deadline_clause(task: &TaskView) -> String {
    if task.remaining.is_zero() {
        format!("at its {} timeout now", fmt_secs(task.deadline))
    } else {
        format!(
            "{} left of its {} timeout",
            fmt_secs(task.remaining),
            fmt_secs(task.deadline)
        )
    }
}

fn fmt_secs(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// The `tool_result` a backgrounded invocation closes with.
///
/// A normal result, not an error: the tool call is over, exactly once, and the
/// model is told plainly that the command outlived it rather than being handed
/// a failure it might retry.
#[must_use]
pub fn background_receipt(task: &TaskView) -> String {
    format!(
        "Backgrounded as task {}. `{}` is still running and this tool call is closed; it stops \
         with {} unless it finishes first. Its result will be reported when it finishes — do not \
         re-run it.",
        task.number,
        task.command,
        deadline_clause(task)
    )
}

/// The transcript body a backgrounded block shows.
#[must_use]
pub fn background_body(task: &TaskView) -> String {
    format!(
        "backgrounded as task {} — `/tasks` lists it, `/fg {}` follows it, `/stop {}` ends it; {}",
        task.number,
        task.number,
        task.number,
        deadline_clause(task)
    )
}

/// What a managed run reported when its command ended.
///
/// These are the fields of upstream's `ExecutionResult` that the completion and
/// read-back surfaces render. For a reserved `run_command` run they are read
/// back from the tool's own `executed` response — the JSON the model receives
/// for a foreground call: `ExecutionResult` itself is serialize-only and
/// non-exhaustive, and the reserved run it describes cannot be waited on from
/// outside, so the response *is* the host's source. Fields upstream adds later
/// are ignored; a field it removes fails the read, which the reporter states
/// rather than rendering a guess. A model-started run is not reserved, so its
/// result arrives as an `ExecutionResult` and is projected by
/// [`from_execution`](Self::from_execution) — one type either way, so nothing
/// downstream has to know which route a task took.
#[derive(Debug, Clone, Deserialize)]
pub struct CommandResult {
    /// Process exit code; `None` when the child was killed.
    pub exit_code: Option<i32>,
    /// Retained standard output.
    pub stdout: String,
    /// Retained standard error.
    pub stderr: String,
    /// Why the run ended.
    #[serde(deserialize_with = "shell_outcome")]
    pub outcome: ShellOutcome,
    /// Wall-clock length of the run.
    pub duration: Duration,
    /// Events that did not reach the sink.
    pub dropped_events: u64,
    /// Whether reaping and reader cleanup finished within the grace period.
    pub cleanup_completed: bool,
    /// Whether in-memory stdout capture hit its limit.
    pub stdout_truncated: bool,
    /// Whether in-memory stderr capture hit its limit.
    pub stderr_truncated: bool,
    /// The complete stdout on disk, when capture overflowed and spilling is on.
    pub stdout_overflow_path: Option<PathBuf>,
    /// The complete stderr on disk, when capture overflowed and spilling is on.
    pub stderr_overflow_path: Option<PathBuf>,
}

impl CommandResult {
    /// Read the result out of `run_command`'s response.
    ///
    /// # Errors
    ///
    /// Says what the response was instead when it is not an `executed` one —
    /// a `confirmation_required` answer never started a run, so a tracked
    /// task cannot end with it — or when its fields cannot be read.
    pub fn from_tool_response(value: &serde_json::Value) -> Result<Self, String> {
        match value.get("status").and_then(serde_json::Value::as_str) {
            Some("executed") => serde_json::from_value(value.clone())
                .map_err(|err| format!("the executed response could not be read: {err}")),
            Some(status) => Err(format!("the response was `{status}`, not `executed`")),
            None => Err("the response named no status".to_string()),
        }
    }

    /// Project the executor's own result, for a run that was never reserved.
    ///
    /// `ExecutionResult` is `#[non_exhaustive]`, so this names every field it
    /// reads and ignores the rest — the same contract as the deserialized
    /// route above.
    #[must_use]
    pub fn from_execution(result: &ExecutionResult) -> Self {
        Self {
            exit_code: result.exit_code,
            stdout: result.stdout.clone(),
            stderr: result.stderr.clone(),
            outcome: result.outcome,
            duration: result.duration,
            dropped_events: result.dropped_events,
            cleanup_completed: result.cleanup_completed,
            stdout_truncated: result.stdout_truncated,
            stderr_truncated: result.stderr_truncated,
            stdout_overflow_path: result.stdout_overflow_path.clone(),
            stderr_overflow_path: result.stderr_overflow_path.clone(),
        }
    }
}

/// `ShellOutcome` as the tool serializes it, read back by its wire name.
fn shell_outcome<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ShellOutcome, D::Error> {
    let name = String::deserialize(deserializer)?;
    match name.as_str() {
        "exited" => Ok(ShellOutcome::Exited),
        "timed_out" => Ok(ShellOutcome::TimedOut),
        "cancelled" => Ok(ShellOutcome::Cancelled),
        other => Err(serde::de::Error::unknown_variant(
            other,
            &["exited", "timed_out", "cancelled"],
        )),
    }
}

/// The lossiness sentence, when this view of the output is not the whole one.
///
/// Empty when nothing was lost. A coalesced tail is never presented as
/// complete output, which is the whole reason the executor counts drops and
/// names an overflow file.
#[must_use]
pub fn lossiness(task: &TaskView, result: Option<&CommandResult>) -> String {
    let dropped = result.map_or(task.dropped, |result| result.dropped_events);
    let mut notes = Vec::new();
    if dropped > 0 {
        notes.push(format!(
            "{dropped} output event(s) were dropped on the way to this view, so the lines above \
             skip some output"
        ));
    }
    let overflow: Vec<String> = result
        .into_iter()
        .flat_map(|result| {
            [
                result.stdout_overflow_path.as_ref(),
                result.stderr_overflow_path.as_ref(),
            ]
        })
        .flatten()
        .map(|path| path.display().to_string())
        .collect();
    if !overflow.is_empty() {
        notes.push(format!(
            "in-memory capture overflowed; the complete output is on disk at {}",
            overflow.join(" and ")
        ));
    } else if task.capture_overflowed
        || result.is_some_and(|r| r.stdout_truncated || r.stderr_truncated)
    {
        notes.push(
            "in-memory capture overflowed, so what is shown is a tail rather than the whole output"
                .to_string(),
        );
    }
    if notes.is_empty() {
        String::new()
    } else {
        format!("\n⚠ {}.", notes.join("; "))
    }
}

/// How a finished task's result was reconciled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconciliation {
    /// Delivered at the graph's mid-turn steering boundary, so the model's
    /// canonical history contains it.
    Steered,
    /// Recorded in the transcript only, because there was no turn to steer.
    TranscriptOnly,
    /// Recorded in the transcript only, because the session that started it is
    /// no longer the current one.
    ForeignSession,
    /// Recorded in the transcript only, because the completion names a path
    /// outside the workspace and the containment gate refused it.
    Uncontained,
}

impl Reconciliation {
    /// The disclosure a transcript-only completion owes the user.
    ///
    /// Silence here would reintroduce exactly the vanishing this design
    /// refuses: the model does not know the command finished, and the user is
    /// the only one who can tell it.
    #[must_use]
    pub fn disclosure(self, task: &TaskView) -> String {
        match self {
            Self::Steered => String::new(),
            Self::TranscriptOnly => {
                "\nRecorded in this transcript only — the session was idle, so the model's history \
                 does not have it. Mention it in your next message if the agent should know."
                    .to_string()
            }
            Self::ForeignSession => format!(
                "\nRecorded in this transcript only — task {} belongs to session `{}`, which is \
                 not the current one, so no model history was written.",
                task.number,
                task.session.as_str()
            ),
            Self::Uncontained => {
                "\nRecorded in this transcript only — the result names a path outside this \
                 workspace, which the workspace-containment gate refuses to put in front of the \
                 model. Use `/task` to read it."
                    .to_string()
            }
        }
    }
}

/// The completion body for a finished task.
#[must_use]
pub fn completion_body(task: &TaskView, result: &CommandResult, how: Reconciliation) -> String {
    let mut body = match result.outcome {
        ShellOutcome::Exited => match result.exit_code {
            Some(0) => format!("task {} finished: exit 0", task.number),
            Some(code) => format!("task {} finished: exit {code}", task.number),
            None => format!("task {} finished with no exit code", task.number),
        },
        ShellOutcome::TimedOut => format!(
            "task {} timed out at its {} deadline and was stopped",
            task.number,
            fmt_secs(task.deadline)
        ),
        ShellOutcome::Cancelled => format!("task {} was stopped", task.number),
    };
    let _ = write!(body, " after {}", fmt_secs(result.duration));
    if !result.cleanup_completed {
        body.push_str("; reaping did not complete within the grace period");
    }
    for line in task.tail.iter().rev().take(3).rev() {
        let _ = write!(body, "\n{line}");
    }
    body.push_str(&lossiness(task, Some(result)));
    body.push_str(&how.disclosure(task));
    body
}

/// The completion, phrased for the model rather than the transcript.
#[must_use]
pub fn completion_steer(task: &TaskView, result: &CommandResult) -> String {
    let outcome = match result.outcome {
        ShellOutcome::Exited => match result.exit_code {
            Some(0) => "exit 0".to_string(),
            Some(code) => format!("exit {code}"),
            None => "no exit code".to_string(),
        },
        ShellOutcome::TimedOut => format!("timed out at its {} deadline", fmt_secs(task.deadline)),
        ShellOutcome::Cancelled => "stopped".to_string(),
    };
    let mut text = format!(
        "Background task {} (`{}`), which was backgrounded during an earlier tool call, has \
         finished: {outcome} after {}.",
        task.number,
        task.command,
        fmt_secs(result.duration)
    );
    let tail: Vec<&str> = task
        .tail
        .iter()
        .rev()
        .take(20)
        .rev()
        .map(String::as_str)
        .collect();
    if !tail.is_empty() {
        let _ = write!(text, "\nLast output:\n{}", tail.join("\n"));
    }
    text.push_str(&lossiness(task, Some(result)));
    text
}

/// The host side of a backgrounded task's life after its turn.
///
/// Holds the handles the composition root already owns — nothing new, and
/// nothing it could use to run a turn. It takes over the wait the turn gave
/// up, sees it to its end, closes the block the invocation left open, and
/// reconciles the result the one way the boundary allows. It holds no
/// executor: the run is reserved for the invocation it waits on, so the
/// executor would refuse it anyway.
#[derive(Clone)]
pub struct BackgroundReporter {
    executor: ShellExecutor,
    output: TerminalOutput,
    steering: TurnSteering,
    tasks: BackgroundTasks,
    current_session: Arc<dyn Fn() -> SessionId + Send + Sync>,
}

impl std::fmt::Debug for BackgroundReporter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BackgroundReporter").finish()
    }
}

impl BackgroundReporter {
    /// Build the reporter from the handles the host already has.
    #[must_use]
    pub fn new(
        executor: ShellExecutor,
        output: TerminalOutput,
        steering: TurnSteering,
        tasks: BackgroundTasks,
        current_session: impl Fn() -> SessionId + Send + Sync + 'static,
    ) -> Self {
        Self {
            executor,
            output,
            steering,
            tasks,
            current_session: Arc::new(current_session),
        }
    }

    /// Watch one backgrounded run to its end, reporting as it goes.
    ///
    /// Returns how the completion was reconciled, so a caller — a test, or a
    /// host that wants to log it — can see which route the boundary allowed
    /// rather than having to infer it from the rendering.
    ///
    /// # Two kinds of run, one waiter
    ///
    /// Both arrive here and they differ only in where the terminal result
    /// comes from:
    ///
    /// - a `run_command` invocation the user pushed out of its turn is
    ///   **reserved** for its `execute` caller, so [`ShellExecutor::wait_for`]
    ///   refuses it and the host was handed the tool's own wait instead
    ///   ([`ToolRun`]); its result is the `executed` response the model would
    ///   have received; and
    /// - a run the model started with `run_in_background` was never reserved
    ///   and has no such wait, so the executor's own `wait_for` is the one
    ///   route to its [`ExecutionResult`].
    ///
    /// Either way there is exactly one waiter — this one — and both results
    /// become a [`CommandResult`], so nothing below this point knows which
    /// route a task took.
    pub async fn watch(&self, number: u64) -> Option<Reconciliation> {
        let task = self.tasks.by_number(number)?;
        let id = task.id;
        let mut waiter: Pin<Box<dyn Future<Output = Result<CommandResult, String>> + Send + '_>> =
            match self.tasks.take_run(number) {
                Some(run) => Box::pin(async move {
                    match run.await {
                        Ok(Ok(value)) => CommandResult::from_tool_response(&value).map_err(|why| {
                            format!("task {number} ended, but its result could not be read: {why}")
                        }),
                        // The tool itself failed the run — the executor refused
                        // or lost it. Say so rather than close the block with an
                        // outcome nobody observed.
                        Ok(Err(err)) => Err(format!("task {number} failed: {err}")),
                        // The wait ended without an answer: the task panicked or
                        // was aborted. Neither is done to a backgrounded run, so
                        // name it.
                        Err(err) => {
                            Err(format!("task {number} ended, but its wait was lost: {err}"))
                        }
                    }
                }),
                None => Box::pin(async move {
                    match self.executor.wait_for(id).await {
                        Some(Ok(result)) => Ok(CommandResult::from_execution(&result)),
                        Some(Err(err)) => Err(format!("task {number} failed: {err}")),
                        None => Err(format!(
                            "task {number} ended, but its result had already been taken; \
                             `/tasks` no longer lists it"
                        )),
                    }
                }),
            };
        let result = loop {
            tokio::select! {
                result = &mut waiter => break result,
                () = tokio::time::sleep(COALESCE_TICK) => self.forward_tail(number).await,
            }
        };
        // Re-read: elapsed time and the tail both moved while we waited.
        let task = self.tasks.by_number(number).unwrap_or(task);
        let result = match result {
            Ok(result) => result,
            Err(detail) => {
                self.close_without_result(&task, &detail).await;
                return None;
            }
        };
        let how = self.reconcile(&task, &result);
        let phase = if matches!(result.outcome, ShellOutcome::Exited) && result.exit_code == Some(0)
        {
            PHASE_OK
        } else {
            PHASE_ERROR
        };
        self.send(tool_message(
            &task.call_id,
            task.parent.as_deref(),
            &task.label,
            phase,
            u64::try_from(result.duration.as_millis()).unwrap_or(u64::MAX),
            &completion_body(&task, &result, how),
        ))
        .await;
        // Keep what the run produced: this waiter observed the only copy, and
        // an idle or foreign-session completion has reached no model history,
        // so this record is the model's only remaining route to the exit code.
        self.tasks.finish(task.id, Some(result), None);
        self.report_running().await;
        Some(how)
    }

    /// Close a block whose run ended with nothing the host can report as an
    /// outcome, and retire the task. The detail says what happened instead.
    ///
    /// The task leaves the live list either way, and the detail is kept as the
    /// record's note: "this task ended and nobody saw its result" and "there
    /// was never such a task" are different answers, and `task_output` has to
    /// be able to give the first one.
    async fn close_without_result(&self, task: &TaskView, detail: &str) {
        self.send(tool_message(
            &task.call_id,
            task.parent.as_deref(),
            &task.label,
            PHASE_ERROR,
            elapsed_ms(task),
            detail,
        ))
        .await;
        self.tasks.finish(task.id, None, Some(detail.to_string()));
        self.report_running().await;
    }

    /// Reconcile a completion the one way the boundary allows.
    ///
    /// Mid-turn, it goes to the graph-owned steering boundary and therefore
    /// reaches `ContextManager`. Idle, it does **not**: a `Steer` submission
    /// with no active run degrades to a queued prompt, which would start a
    /// turn the user never asked for. The host does not write the model's
    /// history itself, so the completion stays in the transcript and says so.
    fn reconcile(&self, task: &TaskView, result: &CommandResult) -> Reconciliation {
        if (self.current_session)() != task.session {
            return Reconciliation::ForeignSession;
        }
        match self.steering.steer_mid_turn(completion_steer(task, result)) {
            Ok(_) => Reconciliation::Steered,
            Err(SteerRefused::NoActiveRun) => Reconciliation::TranscriptOnly,
            Err(SteerRefused::Rejected(rejected)) => {
                tracing::warn!(
                    "background completion not steered into history: {}",
                    rejected.notice()
                );
                Reconciliation::Uncontained
            }
        }
    }

    /// Send one coalesced tail update, when the user is following this task.
    async fn forward_tail(&self, number: u64) {
        if !self.tasks.is_followed(number) {
            return;
        }
        let Some(task) = self.tasks.by_number(number) else {
            return;
        };
        let Some(line) = self.tasks.take_update(&task.call_id) else {
            return;
        };
        self.send(tool_message(
            &task.call_id,
            task.parent.as_deref(),
            &task.label,
            PHASE_RUNNING,
            elapsed_ms(&task),
            &line,
        ))
        .await;
    }

    /// Tell the panel how many tasks are running, so survivors stay visible.
    pub async fn report_running(&self) {
        self.send(polaris_tui::commands::report_status(
            &polaris_tui::commands::StatusReport::default()
                .with_tasks_running(self.tasks.running()),
        ))
        .await;
    }

    async fn send(&self, message: IOMessage) {
        if let Err(err) = self.output.send(message).await {
            tracing::warn!("background task update not delivered: {err}");
        }
    }
}

fn elapsed_ms(task: &TaskView) -> u64 {
    u64::try_from(task.elapsed.as_millis()).unwrap_or(u64::MAX)
}

/// Wait for one abandoned run and retain its terminal result for read-back.
///
/// The reporter-free half of what [`BackgroundReporter::watch`] does. A front
/// end with no transcript to report into — `--print` — still has a model that
/// can ask what its own background command produced, and
/// [`ShellExecutor::wait_for`] consumes the result exactly once, so someone has
/// to be the waiter. This is that someone, and it is used **only** when
/// [`BackgroundTasks::report_backgrounded`] found no reporter to hand the run
/// to, which is what keeps the count at one waiter per run.
///
/// Reports nothing and steers nothing: it writes the record and returns.
pub async fn collect_result(executor: &ShellExecutor, tasks: &BackgroundTasks, number: u64) {
    let Some(task) = tasks.by_number(number) else {
        return;
    };
    let (result, note) = match executor.wait_for(task.id).await {
        Some(Ok(result)) => (Some(CommandResult::from_execution(&result)), None),
        Some(Err(err)) => (None, Some(format!("task {number} failed: {err}"))),
        None => (
            None,
            Some(format!(
                "task {number} ended, but its result had already been taken"
            )),
        ),
    };
    tasks.finish(task.id, result, note);
}

/// The `/tasks`, `/task`, `/fg` and `/stop` commands over a live executor.
///
/// Registered by the host onto the existing [`CommandRegistry`], exactly as
/// session navigation and settings are. They read the executor's own process
/// table and this host table; they hold neither.
#[derive(Clone)]
pub struct BackgroundCommands {
    executor: ShellExecutor,
    tasks: BackgroundTasks,
    output: TerminalOutput,
    current_session: Arc<dyn Fn() -> SessionId + Send + Sync>,
}

impl std::fmt::Debug for BackgroundCommands {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("BackgroundCommands").finish()
    }
}

impl BackgroundCommands {
    /// Build the command set over the executor the host already booted.
    #[must_use]
    pub fn new(
        executor: ShellExecutor,
        tasks: BackgroundTasks,
        output: TerminalOutput,
        current_session: impl Fn() -> SessionId + Send + Sync + 'static,
    ) -> Self {
        Self {
            executor,
            tasks,
            output,
            current_session: Arc::new(current_session),
        }
    }

    /// Register `/tasks`, `/task`, `/fg` and `/stop`.
    ///
    /// # Errors
    ///
    /// As [`CommandRegistry::register`].
    pub fn register(&self, commands: &CommandRegistry) -> Result<(), CommandError> {
        let list = self.clone();
        commands.register_sync(
            CommandSpec::new("tasks", "List background shell tasks")
                .with_usage("[--all]")
                .with_choices(["--all"]),
            move |_, args| list.tasks_command(args),
        )?;

        let inspect = self.clone();
        commands.register_sync(
            CommandSpec::new("task", "Show one background task and its output").with_usage("<id>"),
            move |_, args| inspect.task_command(args),
        )?;

        let follow = self.clone();
        commands.register_sync(
            CommandSpec::new("fg", "Follow a background task's output again").with_usage("<id>"),
            move |_, args| follow.fg_command(args),
        )?;

        let stop = self.clone();
        commands.register(
            CommandSpec::new("stop", "Stop a background task").with_usage("<id>"),
            Arc::new(move |ctx: super::CommandContext<'_>| {
                let stop = stop.clone();
                let args = ctx.args.to_string();
                Box::pin(async move { stop.stop_command(&args).await }) as super::CommandFuture<'_>
            }),
        )
    }

    /// The live task ids, offered as completion values.
    #[must_use]
    fn choices(&self) -> Vec<String> {
        self.tasks
            .all()
            .into_iter()
            .map(|task| task.number.to_string())
            .collect()
    }

    fn tasks_command(&self, args: &str) -> CommandOutcome {
        let args = args.trim();
        let all = matches!(args, "--all" | "-a" | "all");
        if !args.is_empty() && !all {
            return CommandOutcome::Error(format!(
                "`/tasks` takes no argument or `--all`; `{args}` is neither"
            ));
        }
        let current = (self.current_session)();
        let tracked = self.tasks.all();
        let statuses = self.executor.processes();
        let (mine, theirs): (Vec<_>, Vec<_>) =
            tracked.iter().partition(|task| task.session == current);
        let shown: Vec<&TaskView> = if all {
            tracked.iter().collect()
        } else {
            mine.clone()
        };

        let mut body = String::new();
        if shown.is_empty() {
            body.push_str(if all {
                "No shell tasks are running."
            } else {
                "No shell tasks are running in this session."
            });
        } else {
            let _ = writeln!(
                body,
                "| task | state | elapsed | deadline | output | command |"
            );
            let _ = writeln!(body, "|---|---|---|---|---|---|");
            for task in &shown {
                let status = statuses.iter().find(|status| status.id == task.id);
                let bytes = status.map_or(0, |status| {
                    status.stdout_bytes.saturating_add(status.stderr_bytes)
                });
                let session = if task.session == current {
                    String::new()
                } else {
                    format!(" *(session `{}`)*", task.session.as_str())
                };
                let _ = writeln!(
                    body,
                    "| {} | {} | {} | {} | {bytes} B | `{}`{session} |",
                    task.number,
                    status.map_or_else(|| "gone".to_string(), state_label),
                    fmt_secs(task.elapsed),
                    deadline_clause(task),
                    task.command,
                );
            }
        }
        if !all && !theirs.is_empty() {
            let _ = write!(
                body,
                "\n\n{} task(s) from other sessions are hidden; `/tasks --all` lists every session.",
                theirs.len()
            );
        }
        let _ = write!(body, "\n\n{}", self.guarantees());
        CommandOutcome::Reply(body)
    }

    /// What stopping a task actually reaches, and what a task is not.
    fn guarantees(&self) -> String {
        let tree = match ShellExecutor::tree_guarantee() {
            ProcessTreeGuarantee::Group => {
                "`/stop` signals the whole spawned process group, so a task's children stop with it"
            }
            ProcessTreeGuarantee::DirectChildOnly => {
                "`/stop` signals only the direct child on this platform; descendants it spawned may \
                 survive"
            }
        };
        format!(
            "{tree}. These are shell processes, not terminals: no TTY, so no colour, no progress \
             redraw and no interactive input."
        )
    }

    fn task_command(&self, args: &str) -> CommandOutcome {
        let (number, task) = match self.resolve(args) {
            Ok(found) => found,
            Err(outcome) => return outcome,
        };
        let status = self
            .executor
            .processes()
            .into_iter()
            .find(|status| status.id == task.id);
        let mut body = format!(
            "**task {number}** · {} · {} · {}\n\n`{}`\n",
            status
                .as_ref()
                .map_or_else(|| "gone".to_string(), state_label),
            fmt_secs(task.elapsed),
            deadline_clause(&task),
            task.command,
        );
        let _ = write!(
            body,
            "\nStarted by session `{}` at turn {}.\n",
            task.session.as_str(),
            task.turn
        );
        if task.tail.is_empty() {
            body.push_str("\nNo output yet.");
        } else {
            let _ = write!(body, "\n```\n{}\n```", task.tail.join("\n"));
        }
        let notes = lossiness(&task, None);
        if notes.is_empty() {
            body.push_str(
                "\nThis is the last of the output the host retained, not the whole of it.",
            );
        } else {
            body.push_str(&notes);
        }
        CommandOutcome::Reply(body)
    }

    fn fg_command(&self, args: &str) -> CommandOutcome {
        let (number, task) = match self.resolve(args) {
            Ok(found) => found,
            Err(outcome) => return outcome,
        };
        if !self.tasks.set_followed(number, true) {
            return CommandOutcome::Error(format!("no task {number}"));
        }
        CommandOutcome::Reply(format!(
            "Following task {number} again — its block resumes live output; {}. This re-attaches \
             your attention only: the tool call that started it was closed when it was \
             backgrounded and does not reopen.",
            deadline_clause(&task)
        ))
    }

    async fn stop_command(&self, args: &str) -> CommandOutcome {
        let (number, task) = match self.resolve(args) {
            Ok(found) => found,
            Err(outcome) => return outcome,
        };
        let status = self
            .executor
            .processes()
            .into_iter()
            .find(|status| status.id == task.id);
        match status.as_ref().map(|status| status.state) {
            Some(ShellProcessState::Terminating) => {
                return CommandOutcome::Error(format!("task {number} is already stopping"));
            }
            Some(ShellProcessState::Finished(outcome)) => {
                return CommandOutcome::Error(format!(
                    "task {number} already finished ({})",
                    outcome_label(outcome)
                ));
            }
            None => {
                return CommandOutcome::Error(format!(
                    "task {number} is no longer in the process table"
                ));
            }
            Some(ShellProcessState::Running) => {}
        }
        if self.executor.stop(task.id).await {
            CommandOutcome::Reply(format!(
                "Stopping task {number} (`{}`). {}",
                task.command,
                self.guarantees()
            ))
        } else {
            CommandOutcome::Error(format!(
                "task {number} did not accept a stop request; it had already reached a terminal \
                 state"
            ))
        }
    }

    /// Resolve the `<id>` argument the three per-task commands share.
    fn resolve(&self, args: &str) -> Result<(u64, TaskView), CommandOutcome> {
        let args = args.trim();
        if args.is_empty() {
            return Err(CommandOutcome::Error(format!(
                "name a task id; `/tasks` lists them{}",
                match self.choices().as_slice() {
                    [] => String::new(),
                    live => format!(" (running now: {})", live.join(", ")),
                }
            )));
        }
        let Ok(number) = args.parse::<u64>() else {
            return Err(CommandOutcome::Error(format!(
                "`{args}` is not a task id; `/tasks` lists them"
            )));
        };
        self.tasks
            .by_number(number)
            .map(|task| (number, task))
            .ok_or_else(|| CommandOutcome::Error(format!("no task {number}")))
    }

    /// The panel-facing side of the `Ctrl-T` chord.
    ///
    /// Answers a refusal itself and lets the in-flight invocation emit the
    /// receipt, because only the system holding the tool block can close it
    /// exactly once.
    pub async fn request_background(&self) {
        if let Err(refusal) = self.tasks.request()
            && let Err(err) = self.output.send(refusal.into_message()).await
        {
            tracing::warn!("background refusal not delivered: {err}");
        }
    }
}

fn state_label(status: &ShellProcessStatus) -> String {
    match status.state {
        ShellProcessState::Running => "running".to_string(),
        ShellProcessState::Terminating => "stopping".to_string(),
        ShellProcessState::Finished(outcome) => outcome_label(outcome),
    }
}

fn outcome_label(outcome: ShellOutcome) -> String {
    match outcome {
        ShellOutcome::Exited => "exited".to_string(),
        ShellOutcome::TimedOut => "timed out".to_string(),
        ShellOutcome::Cancelled => "cancelled".to_string(),
    }
}
