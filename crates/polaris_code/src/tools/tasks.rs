//! The model's own route to a background shell run, and back.
//!
//! # Why this is a toolset and not a `run_command` parameter
//!
//! `run_command` belongs to `polaris_shell`, and its three model-visible
//! arguments (`command`, `working_dir`, `timeout_secs`) are fixed by the
//! `#[tool]` macro at the definition site upstream. A consumer cannot amend a
//! registered tool's schema: [`ToolRegistry`] exposes `set_permission`,
//! `set_strict` and `set_exposed` and nothing that rewrites parameters, and by
//! the time `Server::finish()` returns the registry has moved into a global and
//! is no longer mutable at all. So `run_command { run_in_background: true }`
//! would be a `polaris-ai` change, which a consumer of the framework cannot
//! make. A polaris-code toolset over the executor the shell plugin already
//! published is the composition that stays in this repository.
//!
//! # What it composes
//!
//! Nothing here is new machinery:
//!
//! | Need | Existing primitive |
//! |---|---|
//! | Start a process and not wait for it | [`ShellExecutor::start`] plus `ShellProcess::detach`, which leaves the process under executor ownership and returns its identity. |
//! | Stream its output somewhere the host can read | the [`ShellSinkHandle`] `execute_tools` already installs in the tool context — the same `TaskSink` a `Ctrl-T` run uses. |
//! | Remember the run | [`BackgroundTasks`], the table the chord and the four `/commands` already share. |
//! | Keep the exit code | [`BackgroundTasks::finished`], written by whichever single waiter the host runs. |
//! | Stop it | [`ShellExecutor::stop`], the same call `/stop` makes. |
//!
//! There is no second process registry, no scheduler and no lifecycle type.
//!
//! # The permission posture
//!
//! Backgrounding is not a way around a gate. Three things hold together:
//!
//! 1. `run_in_background` resolves through the same
//!    [`PermissionPolicy`](crate::agents::PermissionPolicy) seam as
//!    `run_command`, and every shipped
//!    [`PermissionMode`](crate::agents::PermissionMode) names the two with one
//!    entry — see `SHELL_EXECUTION_TOOLS` in `agents/policy.rs`.
//! 2. [`ShellExecutor::start`] applies the very `check_permission` call
//!    [`ShellExecutor::execute`] applies, so a command's `denied_commands` /
//!    `allowed_commands` verdict is identical whichever tool asked for it.
//! 3. A `Confirm` verdict comes back in the shape `run_command` returns it —
//!    `{"status": "confirmation_required", …}` — so the unattended
//!    fail-closed check in `execute_tools` refuses it there rather than this
//!    tool inventing a second answer.
//!
//! # Lifetime
//!
//! A background task belongs to the **process**, not to the turn, the tool call
//! or the session:
//!
//! - **the turn that started it ends** — the task keeps running. That is the
//!   point, and it is why the turn's cancellation observer is deliberately not
//!   attached to the request: `Ctrl-C` ends the turn, not the task.
//! - **the session is switched** — the task keeps running and stays readable.
//!   The transcript-side reconciliation already refuses to write another
//!   session's history and says so; hiding the task from the model as well
//!   would manufacture exactly the orphan this surface exists to prevent, so
//!   `task_output` lists every task this process started.
//! - **the process exits** — the task dies with it. `ShellPlugin::cleanup`
//!   calls `ShellExecutor::stop_all`, which signals the whole spawned process
//!   group on Unix. Nothing started here survives `polaris-code`.

use std::fmt::Write as _;
use std::time::Duration;

use polaris::shell::{
    ShellError, ShellExecutor, ShellOutcome, ShellPlugin, ShellProcessState, ShellRequest,
    ShellSinkHandle, ShellStartResponse,
};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};

use super::approval::ApprovalReceipt;

use crate::terminal::background::{
    BACKGROUND_DEFAULT_TIMEOUT_SECS, BackgroundTasks, FINISHED_RETAINED, FinishedTask, TaskView,
    collect_result, deadline_clause, lossiness,
};

/// The tool that starts a command without waiting for it.
pub const RUN_IN_BACKGROUND: &str = "run_in_background";
/// The tool that reads a background task back.
pub const TASK_OUTPUT: &str = "task_output";
/// The tool that ends a background task early.
pub const STOP_TASK: &str = "stop_task";

/// Most bytes of one finished stream handed back in a single read.
///
/// The executor's capture is already bounded by `ShellConfig::max_output_bytes`
/// (64 KiB in `polaris-code`), but 64 KiB of build log is most of a context
/// window. This is the read-back budget, and a response that spends it says so
/// and says how much it left out — the tail is what is kept, because the end of
/// a build log is the part that says why it failed.
const OUTPUT_BUDGET: usize = 8 * 1024;

/// Background shell tasks the model starts, reads and stops.
///
/// Holds the executor the shell plugin published and the table the `Ctrl-T`
/// chord and the four `/commands` already share: one process table, one task
/// table, two ways in.
#[derive(Debug, Clone)]
pub struct TaskTools {
    executor: ShellExecutor,
    tasks: BackgroundTasks,
}

impl TaskTools {
    /// Build the toolset over an executor and the host's task table.
    #[must_use]
    pub fn new(executor: ShellExecutor, tasks: BackgroundTasks) -> Self {
        Self { executor, tasks }
    }
}

#[toolset]
impl TaskTools {
    /// Start a shell command in the background and return straight away.
    ///
    /// Use this instead of `run_command` for anything slow — a test suite, a
    /// build, a dev server — because `run_command` blocks until the command
    /// finishes. The reply names a task number; pass it to `task_output` to see
    /// what the command is doing and, once it has ended, its exit status. The
    /// task keeps running across turns and ends only when it finishes, when
    /// `stop_task` stops it, or when its timeout expires.
    #[tool]
    async fn run_in_background(
        &self,
        /// The shell command to execute.
        command: String,
        /// Seconds before the task is stopped. Defaults to 3600 (one hour).
        #[default(None::<u64>)]
        timeout_secs: Option<u64>,
        /// Host destination for incremental process events.
        #[context]
        sink: Option<ShellSinkHandle>,
        /// Single-use host authorization for these exact arguments.
        #[context]
        approval: Option<ApprovalReceipt>,
    ) -> Result<serde_json::Value, ToolError> {
        let seconds = timeout_secs.unwrap_or(BACKGROUND_DEFAULT_TIMEOUT_SECS);
        let mut request = ShellRequest::new(&command).with_timeout(seconds);
        if let Some(sink) = sink {
            request = request.with_sink(sink);
        }
        // No `ShellCancel`. The turn's token is wired into a foreground
        // `run_command` so Ctrl-C can stop it; a task that outlives its turn by
        // construction must not be taken down by the turn ending.
        let response = if let Some(approval) = approval {
            let Some(directory) = approval.consume_background(&command, timeout_secs) else {
                return Err(ToolError::permission_denied(
                    "Approval does not match this background invocation",
                ));
            };
            self.executor
                .start_confirmed(request.with_working_dir(directory))
                .await
                .map(ShellStartResponse::Started)
        } else {
            self.executor.start(request).await
        };
        let started = match response {
            Ok(ShellStartResponse::Started(process)) => process.detach(),
            Ok(ShellStartResponse::ConfirmationRequired(request)) => {
                return Ok(serde_json::json!({
                    "status": "confirmation_required",
                    "command": request.command,
                    "timeout_secs": request.timeout_secs,
                }));
            }
            Err(ShellError::PermissionDenied(command)) => {
                return Err(ToolError::permission_denied(format!(
                    "Command denied by policy: {command}"
                )));
            }
            Err(other) => return Err(ToolError::execution_error(other.to_string())),
        };
        let Some(task) = self.tasks.background_process(started) else {
            // The executor emits `Started` synchronously before `start`
            // returns, so the table has the run by now unless this host wired
            // no sink at all.
            return Err(ToolError::execution_error(
                "the command started but this host is not tracking background tasks; use \
                 run_command instead"
                    .to_string(),
            ));
        };
        // Exactly one waiter per run, because `wait_for` consumes the terminal
        // result: the host's reporter when there is one, this collector when
        // there is not.
        if !self.tasks.report_backgrounded(task.number) {
            let executor = self.executor.clone();
            let tasks = self.tasks.clone();
            let number = task.number;
            tokio::spawn(async move { collect_result(&executor, &tasks, number).await });
        }
        Ok(serde_json::Value::String(format!(
            "Started task {} in the background: `{}`. It stops {}. Keep working; call \
             `task_output` with task {} for its output and exit status, or `stop_task` to end it \
             early. Do not re-run this command.",
            task.number,
            task.command,
            deadline_clause(&task),
            task.number,
        )))
    }

    /// Report a background task's state, output and exit status.
    ///
    /// Omit `task` to list every background task this session knows about,
    /// running and finished. Naming one returns its output — the retained tail
    /// while it runs, the captured output once it has ended — with its exit
    /// status and a note whenever what is shown is not the whole output.
    #[tool]
    async fn task_output(
        &self,
        /// The task number reported when the task was started. Omit to list all.
        #[default(None::<u64>)]
        task: Option<u64>,
    ) -> Result<String, ToolError> {
        match task {
            None => Ok(self.render_list()),
            Some(number) => self
                .render_one(number)
                .ok_or_else(|| ToolError::parameter_error(self.unknown_task(number))),
        }
    }

    /// Stop a running background task.
    ///
    /// Signals the task's whole process group where the platform allows it. Its
    /// output stays readable with `task_output` afterwards.
    #[tool]
    async fn stop_task(
        &self,
        /// The task number reported when the task was started.
        task: u64,
    ) -> Result<String, ToolError> {
        if let Some(record) = self.tasks.finished_by_number(task) {
            return Ok(format!(
                "Task {task} had already ended ({}); nothing to stop.",
                terminal_phrase(&record)
            ));
        }
        let Some(view) = self.tasks.by_number(task) else {
            return Err(ToolError::parameter_error(self.unknown_task(task)));
        };
        if self.executor.stop(view.id).await {
            Ok(format!(
                "Stopping task {task} (`{}`). Its output stays readable with `task_output`.",
                view.command
            ))
        } else {
            Ok(format!(
                "Task {task} (`{}`) did not accept a stop request; it had already reached a \
                 terminal state. Call `task_output` for its result.",
                view.command
            ))
        }
    }
}

impl TaskTools {
    /// The error body for a task number nothing knows about.
    ///
    /// Names what *is* known, because "there is no task 7" on its own cannot be
    /// told apart from "the read-back ring dropped task 7".
    fn unknown_task(&self, number: u64) -> String {
        let mut known: Vec<String> = self
            .tasks
            .all()
            .iter()
            .map(|task| task.number.to_string())
            .collect();
        known.extend(
            self.tasks
                .finished()
                .iter()
                .map(|record| record.task.number.to_string()),
        );
        let preamble = if known.is_empty() {
            "no background task started in this process has that number".to_string()
        } else {
            format!("known task numbers are {}", known.join(", "))
        };
        format!(
            "no background task {number}: {preamble}. Only the last {FINISHED_RETAINED} tasks to \
             finish are retained for read-back."
        )
    }

    /// Every task, running and retained, one line each.
    fn render_list(&self) -> String {
        let running = self.tasks.all();
        let finished = self.tasks.finished();
        if running.is_empty() && finished.is_empty() {
            return "No background tasks. Start one with `run_in_background`.".to_string();
        }
        let mut body = String::new();
        for view in &running {
            let _ = writeln!(
                body,
                "task {} · {} · stops {}: `{}`{}",
                view.number,
                self.live_state(view),
                deadline_clause(view),
                view.command,
                view.last_line
                    .as_deref()
                    .map(|line| format!("\n  last output: {line}"))
                    .unwrap_or_default(),
            );
        }
        for record in &finished {
            let _ = writeln!(
                body,
                "task {} · {}: `{}`",
                record.task.number,
                terminal_phrase(record),
                record.task.command,
            );
        }
        let _ = write!(
            body,
            "\nCall `task_output` with a task number for its output. Only the last \
             {FINISHED_RETAINED} tasks to finish are retained."
        );
        body
    }

    /// One task in full, whether it is running or retained.
    fn render_one(&self, number: u64) -> Option<String> {
        if let Some(record) = self.tasks.finished_by_number(number) {
            return Some(render_finished(&record));
        }
        let view = self.tasks.by_number(number)?;
        let mut body = format!(
            "task {} · {} · stops {}: `{}`\n",
            view.number,
            self.live_state(&view),
            deadline_clause(&view),
            view.command,
        );
        if view.tail.is_empty() {
            body.push_str("\nNo output yet.");
        } else {
            let _ = write!(
                body,
                "\nThe last {} line(s) this host retained while it runs:\n{}",
                view.tail.len(),
                view.tail.join("\n")
            );
        }
        // Said every time, not only when something was measurably lost: a
        // running task's tail is a window by construction, and presenting it
        // without that clause is the "partial log shown as complete" this
        // surface exists to rule out.
        body.push_str(
            "\nThis is a bounded tail rather than the whole output; the full capture is reported \
             once the task ends.",
        );
        body.push_str(&lossiness(&view, None));
        Some(body)
    }

    /// What the executor says about a run this host still lists as live.
    fn live_state(&self, view: &TaskView) -> String {
        match self.executor.process(view.id).map(|status| status.state) {
            Some(ShellProcessState::Running) => format!("running for {}", fmt_secs(view.elapsed)),
            Some(ShellProcessState::Terminating) => "stopping".to_string(),
            // Terminal at the executor but not yet retained here: the single
            // waiter is between `wait_for` returning and the record landing.
            Some(ShellProcessState::Finished(outcome)) => {
                format!(
                    "{}, result not yet collected",
                    outcome_phrase(outcome, None)
                )
            }
            None => "gone from the process table".to_string(),
        }
    }
}

/// One retained task in full: exit status, captured output, honesty clause.
fn render_finished(record: &FinishedTask) -> String {
    let mut body = format!(
        "task {} · {}: `{}`\n",
        record.task.number,
        terminal_phrase(record),
        record.task.command,
    );
    let Some(result) = &record.result else {
        if let Some(note) = &record.note {
            let _ = write!(body, "\n{note}.");
        }
        let _ = write!(
            body,
            "\nThe last output this host retained before it ended:\n{}",
            if record.task.tail.is_empty() {
                "(none)".to_string()
            } else {
                record.task.tail.join("\n")
            }
        );
        body.push_str(&lossiness(&record.task, None));
        return body;
    };
    let mut streams = String::new();
    append_stream(&mut streams, "stdout", &result.stdout);
    append_stream(&mut streams, "stderr", &result.stderr);
    if streams.is_empty() {
        body.push_str("\nThe task produced no output.");
    } else {
        body.push_str(&streams);
    }
    body.push_str(&lossiness(&record.task, Some(result)));
    body
}

/// Append one captured stream, spending at most [`OUTPUT_BUDGET`] on it.
///
/// Keeps the tail, and when it drops anything it says how many bytes of how
/// many it is showing rather than presenting the remainder as the whole.
fn append_stream(body: &mut String, name: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let total = text.len();
    if total <= OUTPUT_BUDGET {
        let _ = write!(body, "\n--- {name} ({total} bytes) ---\n{text}");
        return;
    }
    let mut start = total - OUTPUT_BUDGET;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let shown = &text[start..];
    let _ = write!(
        body,
        "\n--- {name}: the last {} of {total} bytes; {start} earlier bytes are not shown ---\n\
         {shown}",
        shown.len(),
    );
}

/// One clause naming how a retained task ended.
fn terminal_phrase(record: &FinishedTask) -> String {
    match &record.result {
        Some(result) => format!(
            "{} after {}",
            outcome_phrase(result.outcome, result.exit_code),
            fmt_secs(result.duration)
        ),
        None => "ended; its result was not observed".to_string(),
    }
}

/// The executor's outcome as a sentence fragment.
fn outcome_phrase(outcome: ShellOutcome, exit_code: Option<i32>) -> String {
    match outcome {
        ShellOutcome::Exited => match exit_code {
            Some(code) => format!("exited with code {code}"),
            None => "exited with no exit code".to_string(),
        },
        ShellOutcome::TimedOut => "hit its timeout and was stopped".to_string(),
        ShellOutcome::Cancelled => "was stopped".to_string(),
    }
}

/// Seconds as `12s` or `3m07s`, matching what the `/commands` print.
fn fmt_secs(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Plugin that registers [`TaskTools`] with the [`ToolRegistry`].
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Registers tools on the existing [`ToolRegistry`] over the [`ShellExecutor`] [`ShellPlugin`] published and the host's [`BackgroundTasks`]; provides no resource of its own. |
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — owns the [`ToolRegistry`] this plugin populates.
/// - [`ShellPlugin`] — publishes the [`ShellExecutor`] global these tools
///   drive. Constructing a second executor here would mean a second process
///   table, which is precisely what makes a task the harness has forgotten
///   while the operating system still runs it possible.
///
/// # Tools Provided
///
/// | Tool | Description |
/// |------|-------------|
/// | `run_in_background` | Starts a shell command through [`ShellExecutor::start`], detaches it, and returns its task number without waiting. |
/// | `task_output` | Lists background tasks, or reports one task's state, retained output and exit status. |
/// | `stop_task` | Requests termination of one running background task. |
///
/// # Permissions
///
/// `run_in_background` and `stop_task` register as [`ToolPermission::Confirm`];
/// `task_output` only reads host state and registers as
/// [`ToolPermission::Allow`]. The registry default is a floor: every shipped
/// permission mode overrides `run_in_background` alongside `run_command`, so
/// backgrounding can never be more permissive than running the same command in
/// the foreground.
///
/// # Lifecycle
///
/// - **`build()`** — clones the [`ShellExecutor`] global and registers the
///   toolset. No `ready()` override, no tick schedules and no `cleanup()`: the
///   processes belong to [`ShellPlugin`], which already stops them.
pub struct TaskToolsPlugin {
    tasks: BackgroundTasks,
}

impl TaskToolsPlugin {
    /// Build the plugin over the host's background-task table.
    #[must_use]
    pub fn new(tasks: BackgroundTasks) -> Self {
        Self { tasks }
    }
}

impl Plugin for TaskToolsPlugin {
    const ID: &'static str = "polaris_code::task_tools";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>(), PluginId::of::<ShellPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let executor = server
            .get_global::<ShellExecutor>()
            .map(|executor| ShellExecutor::clone(&executor))
            .expect("ShellPlugin must be added before TaskToolsPlugin");
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before TaskToolsPlugin");
        registry.register_toolset(TaskTools::new(executor, self.tasks.clone()));
        for (name, permission) in [
            (RUN_IN_BACKGROUND, ToolPermission::Confirm),
            (STOP_TASK, ToolPermission::Confirm),
            (TASK_OUTPUT, ToolPermission::Allow),
        ] {
            registry
                .set_permission(name, permission)
                .expect("TaskTools registers every name it gates");
        }
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use polaris::shell::{ShellConfig, ShellSinkHandle};
    use polaris::tools::ToolContext;
    use serde_json::json;

    #[tokio::test]
    async fn host_approval_starts_background_work_without_agent_policy_state() {
        let directory = tempfile::tempdir().unwrap();
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_working_dir(directory.path())
                .with_allowed_dirs(vec![directory.path().into()]),
        );
        let tasks = BackgroundTasks::detached(Duration::from_secs(5));
        let mut registry = ToolRegistry::new();
        registry.register_toolset(TaskTools::new(executor.clone(), tasks.clone()));
        let invocation = tasks.enter("host-background", RUN_IN_BACKGROUND, None);
        let sink = invocation.sink(
            &polaris::sessions::SessionId::from_string("test"),
            0,
            Duration::from_secs(5),
        );
        let args = json!({"command":"printf x >> marker; echo \"$(pwd)\"", "timeout_secs":5});
        let context = ToolContext::new().with(ShellSinkHandle::new(sink));
        assert_eq!(
            registry
                .execute_with(RUN_IN_BACKGROUND, &args, &context)
                .await
                .unwrap()["status"],
            "confirmation_required"
        );
        assert!(!directory.path().join("marker").exists());
        let receipt = ApprovalReceipt::issue(
            RUN_IN_BACKGROUND,
            args.clone(),
            directory.path(),
            &directory.path().canonicalize().unwrap(),
            || true,
        );
        let context = context.with(receipt);
        assert!(
            registry
                .execute_with(RUN_IN_BACKGROUND, &args, &context)
                .await
                .unwrap()
                .as_str()
                .unwrap()
                .contains("Started task")
        );
        assert!(
            registry
                .execute_with(RUN_IN_BACKGROUND, &args, &context)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while tasks.finished().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(directory.path().join("marker")).unwrap(),
            b"x"
        );
        executor.stop_all().await;
    }
}
