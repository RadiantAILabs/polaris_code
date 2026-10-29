//! Coding payload types that cross the coding graphs' boundaries.
//!
//! These are the types a Cosmos [`TypeExplanationRegistry`] must be able to
//! explain for a coding registration to be mechanically admissible. They are
//! all owned here: an embodiment can only explain boundaries it can name, so
//! every type on a coding graph's signature has to be a `polaris_code` type.
//!
//! [`TypeExplanationRegistry`]: polaris_cosmos::topology::TypeExplanationRegistry

use std::collections::VecDeque;
use std::io::Read;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use polaris::system::resource::LocalResource;
use polaris_cosmos::contracts::Status;
use serde::{Deserialize, Serialize};

use super::edit::{
    EditError, EditOutcome, StubSite, apply_exact_edit, read_relative, scan_stub_sites,
};
use super::task::{CodingTask, CodingTaskError, ExactEdit};
use crate::tools::CodeToolsConfig;

/// One adjudicated coding site: the subject every coding assessment is about.
///
/// This is the `Subject` type parameter of every
/// [`Assessment`](polaris_cosmos::contracts::Assessment) a coding topology
/// produces. The coding specifics live here, inside the type parameter, which
/// is exactly why the contract above it stays domain-neutral.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EditSite {
    /// Path relative to the sandbox root.
    pub path: String,
    /// One-based line of the site, or zero when the site is a whole file.
    pub line: usize,
    /// Whether the site is implemented, still stubbed, or could not be judged.
    pub status: Status,
}

impl EditSite {
    /// Creates a site verdict.
    #[must_use]
    pub const fn new(path: String, line: usize, status: Status) -> Self {
        Self { path, line, status }
    }

    /// Creates an unjudged site from a scan hit.
    #[must_use]
    pub fn pending(site: &StubSite) -> Self {
        Self {
            path: site.path.clone(),
            line: site.line,
            status: Status::Indeterminate,
        }
    }
}

/// What a probe observed at one site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteObservation {
    /// The site that was read.
    pub path: String,
    /// Whether the stub marker was still present when read.
    pub marker_present: bool,
    /// Bytes read.
    pub bytes: usize,
}

/// The expected post-edit content of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedFile {
    /// Path the prediction is about.
    pub path: String,
    /// Full file content this embodiment expects to see after the edit.
    pub expected: String,
}

/// The expected post-edit file contents, committed *before* acting.
///
/// This is what makes the prediction-error loop a real prediction-error loop
/// for coding: the expectation is written down first and is falsifiable
/// against the bytes on disk afterwards. A unit of work that states no edits
/// predicts no files, which is a real prediction — "nothing changes" — and is
/// falsified just as readily.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedEdit {
    /// One prediction per stated edit, in the order the edits were stated.
    pub files: Vec<PredictedFile>,
    /// Whether every stated edit's search text was present when the
    /// prediction was made. Vacuously true when nothing was stated.
    pub search_present: bool,
}

/// What applying the stated edits actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    /// One outcome per stated edit, in the order the edits were applied.
    pub outcomes: Vec<EditOutcome>,
}

/// One file as re-read from disk after acting — the ground truth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    /// Path that was re-read.
    pub path: String,
    /// Content actually on disk.
    pub actual: String,
}

/// The edited files as re-read from disk after acting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedEdit {
    /// One re-read per stated edit, in the order the edits were stated.
    pub files: Vec<VerifiedFile>,
}

impl PredictedEdit {
    /// Returns whether the disk matched this prediction exactly.
    #[must_use]
    pub fn matches(&self, verified: &VerifiedEdit) -> bool {
        self.files.len() == verified.files.len()
            && self
                .files
                .iter()
                .zip(verified.files.iter())
                .all(|(predicted, actual)| {
                    predicted.path == actual.path && predicted.expected == actual.actual
                })
    }
}

/// Prediction error for one loop iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditFeedback {
    /// Iterations completed so far.
    pub iteration: usize,
    /// Whether the disk matched the committed prediction exactly.
    pub matched: bool,
}

/// Progress of the hand-built worklist sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepProgress {
    /// Sites still queued.
    pub remaining: usize,
    /// Sites adjudicated so far.
    pub visited: usize,
    /// Remaining iteration budget.
    pub budget_left: usize,
}

/// Path a site verdict is filed under when a unit of work names no file.
///
/// A conclusion still has to be *about* something, and the sandbox root is the
/// honest subject when the caller stated no edit and declared no scope.
const WORKSPACE_ROOT_SITE: &str = ".";

/// The coding workspace a topology acts through.
///
/// Held as `Res<CodingWorkspace>`: applying an edit mutates the *filesystem*,
/// not this record, so shared access is correct. The attempt counter and the
/// role trace use interior mutability so a loop can record real progress
/// without the resource needing write access.
///
/// It holds the stated edits and the stub marker, and deliberately **not**
/// the success check: a [`SuccessCheck::Command`] is a program, and a graph
/// running inside the pristine context must not be able to reach one. The
/// host runs that check afterwards, in [`verify`](super::verify).
///
/// [`SuccessCheck::Command`]: super::task::SuccessCheck::Command
#[derive(Debug, Clone)]
pub struct CodingWorkspace {
    config: CodeToolsConfig,
    edits: Vec<ExactEdit>,
    marker: Option<String>,
    attempts: Arc<AtomicUsize>,
    trace: Arc<Mutex<Vec<String>>>,
}

impl LocalResource for CodingWorkspace {}

impl CodingWorkspace {
    /// Binds a workspace to one sandboxed unit of coding work.
    ///
    /// This is the constructor both the live host and the offline proof use,
    /// so the pristine context a topology runs in is built one way only.
    ///
    /// # Panics
    ///
    /// Never: the task's edits and marker were validated at construction.
    #[must_use]
    pub fn for_task(task: &CodingTask) -> Self {
        Self {
            config: task.tools_config(),
            edits: task.edits().to_vec(),
            marker: task.stub_marker().map(str::to_owned),
            attempts: Arc::new(AtomicUsize::new(0)),
            trace: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Binds a workspace to one sandboxed exact-string edit.
    ///
    /// The narrow form, kept for callers that have the edit in hand and no
    /// task. An empty `marker` means no scan, not a scan for nothing.
    ///
    /// # Errors
    ///
    /// Returns an error when the edit is a no-op or names a blank file.
    pub fn new(
        config: CodeToolsConfig,
        file: impl Into<String>,
        find: impl Into<String>,
        replace: impl Into<String>,
        marker: impl Into<String>,
    ) -> Result<Self, CodingTaskError> {
        let marker = marker.into();
        Ok(Self {
            config,
            edits: vec![ExactEdit::new(file, find, replace)?],
            marker: (!marker.is_empty()).then_some(marker),
            attempts: Arc::new(AtomicUsize::new(0)),
            trace: Arc::new(Mutex::new(Vec::new())),
        })
    }

    /// Returns the sandbox configuration.
    #[must_use]
    pub const fn config(&self) -> &CodeToolsConfig {
        &self.config
    }

    /// Returns the stated edits, which may be none.
    #[must_use]
    pub fn edits(&self) -> &[ExactEdit] {
        &self.edits
    }

    /// Returns the stub marker, when the unit of work named one.
    #[must_use]
    pub fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    /// Returns whether any stated edit targets `path`.
    #[must_use]
    pub fn targets_path(&self, path: &str) -> bool {
        self.edits.iter().any(|edit| edit.file() == path)
    }

    /// Reads any sandbox-relative file.
    ///
    /// # Errors
    ///
    /// Returns an error if the path escapes the sandbox or cannot be read.
    pub fn read(&self, path: &str) -> Result<String, EditError> {
        read_relative(&self.config, path)
    }

    /// Scans the working tree for the stub sites that remain right now.
    ///
    /// Empty when no marker was named.
    #[must_use]
    pub fn stub_sites(&self) -> Vec<StubSite> {
        self.marker()
            .map(|marker| scan_stub_sites(self.config.working_dir.as_path(), marker))
            .unwrap_or_default()
    }

    /// Commits the expected post-edit content before acting.
    ///
    /// Successive edits to one file are predicted in order, so two edits to
    /// the same file predict the file after both, not after each separately.
    ///
    /// # Errors
    ///
    /// Returns an error if a target file cannot be read.
    pub fn predict(&self) -> Result<PredictedEdit, EditError> {
        let mut files: Vec<PredictedFile> = Vec::with_capacity(self.edits.len());
        let mut search_present = true;
        for edit in &self.edits {
            let current = match files.iter().rev().find(|file| file.path == edit.file()) {
                Some(file) => file.expected.clone(),
                None => self.read(edit.file())?,
            };
            let found = current.contains(edit.find());
            search_present &= found;
            let expected = if found {
                current.replacen(edit.find(), edit.replace(), 1)
            } else {
                current
            };
            files.push(PredictedFile {
                path: edit.file().to_owned(),
                expected,
            });
        }
        Ok(PredictedEdit {
            files,
            search_present,
        })
    }

    /// Applies every stated edit to the real working tree, in order.
    ///
    /// A unit of work that states no edits applies none and succeeds: the
    /// graph's job was to run, and the verdict is the success check's.
    ///
    /// # Errors
    ///
    /// Returns an error if an edit is not a unique match, its search text is
    /// absent with no replacement already present, or a write fails.
    pub fn apply(&self) -> Result<AppliedEdit, EditError> {
        let mut outcomes = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            outcomes.push(apply_exact_edit(
                &self.config,
                edit.file(),
                edit.find(),
                edit.replace(),
            )?);
        }
        Ok(AppliedEdit { outcomes })
    }

    /// Re-reads every edited file from disk.
    ///
    /// # Errors
    ///
    /// Returns an error if a target file cannot be read.
    pub fn verify(&self) -> Result<VerifiedEdit, EditError> {
        let mut files = Vec::with_capacity(self.edits.len());
        for edit in &self.edits {
            files.push(VerifiedFile {
                path: edit.file().to_owned(),
                actual: self.read(edit.file())?,
            });
        }
        Ok(VerifiedEdit { files })
    }

    /// Judges the re-read files against the stated edits.
    ///
    /// [`Status::Pass`] requires every replacement present *and* every search
    /// text gone: "the edits landed" is a stronger claim than "the files
    /// changed". With no stated edits an edit-verification graph has not
    /// established the requested work, so it reports [`Status::Issue`]. A
    /// read-only survey or question must instead use its own verifier and
    /// emit a pass when it establishes that no change was needed.
    #[must_use]
    pub fn judge_verified(&self, verified: &VerifiedEdit) -> Status {
        if self.edits.is_empty() {
            return Status::Issue;
        }
        if self.edits.len() != verified.files.len() {
            return Status::Issue;
        }
        let landed = self
            .edits
            .iter()
            .zip(verified.files.iter())
            .all(|(edit, file)| {
                file.path == edit.file()
                    && file.actual.contains(edit.replace())
                    && !file.actual.contains(edit.find())
            });
        if landed { Status::Pass } else { Status::Issue }
    }

    /// Reports whether the caller supplied any stated edit for the separate
    /// applied-edit observation.
    ///
    /// Unlike [`Self::judge_verified`], this does not claim that the task was
    /// completed. It answers only whether that particular observation applies,
    /// so a task described without exact edits remains
    /// [`Status::NotApplicable`] here while its graph conclusion remains
    /// unresolved until another verifier establishes success.
    #[must_use]
    pub fn applied_edit_status(&self, verified: &VerifiedEdit) -> Status {
        if self.edits.is_empty() {
            Status::NotApplicable
        } else {
            self.judge_verified(verified)
        }
    }

    /// Judges one file's content against the edits that target it.
    ///
    /// Used by the sweep, which visits one site at a time.
    #[must_use]
    pub fn judge_path(&self, path: &str, actual: &str) -> Status {
        let mut targeted = self
            .edits
            .iter()
            .filter(|edit| edit.file() == path)
            .peekable();
        if targeted.peek().is_none() {
            return Status::NotApplicable;
        }
        if targeted.all(|edit| actual.contains(edit.replace()) && !actual.contains(edit.find())) {
            Status::Pass
        } else {
            Status::Issue
        }
    }

    /// Applies the edits that target `path` and judges the result.
    ///
    /// # Errors
    ///
    /// Returns an error if an edit cannot be applied or the file cannot be
    /// re-read.
    pub fn apply_path(&self, path: &str) -> Result<Status, EditError> {
        for edit in self.edits.iter().filter(|edit| edit.file() == path) {
            apply_exact_edit(&self.config, path, edit.find(), edit.replace())?;
        }
        let actual = self.read(path)?;
        Ok(self.judge_path(path, &actual))
    }

    /// Judges a swept site by whether the stub marker survives in it.
    ///
    /// With no marker there is nothing to survive, so the site passes.
    #[must_use]
    pub fn judge_marker(&self, content: &str) -> Status {
        match self.marker() {
            Some(marker) if content.contains(marker) => Status::Issue,
            _ => Status::Pass,
        }
    }

    /// Returns whether the stub marker is present in `content`.
    #[must_use]
    pub fn marker_present(&self, content: &str) -> bool {
        self.marker().is_some_and(|marker| content.contains(marker))
    }

    /// Returns one site per stated edit, each carrying `status`.
    ///
    /// A unit of work that stated no edits still gets exactly one site, filed
    /// against the sandbox root, so a conclusion is never empty.
    #[must_use]
    pub fn edit_sites(&self, status: Status) -> Vec<EditSite> {
        if self.edits.is_empty() {
            return vec![EditSite::new(WORKSPACE_ROOT_SITE.to_owned(), 0, status)];
        }
        let mut paths: Vec<&str> = self.edits.iter().map(ExactEdit::file).collect();
        paths.dedup();
        paths
            .into_iter()
            .map(|path| EditSite::new(path.to_owned(), 0, status))
            .collect()
    }

    /// Returns the path a whole-workspace verdict is filed against.
    #[must_use]
    pub fn subject_path(&self) -> &str {
        self.edits
            .first()
            .map_or(WORKSPACE_ROOT_SITE, ExactEdit::file)
    }

    /// Records one completed loop iteration and returns the new count.
    pub fn record_attempt(&self) -> usize {
        self.attempts.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Returns how many loop iterations were recorded.
    #[must_use]
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    /// Appends a role marker to the observable execution trace.
    ///
    /// A poisoned lock is ignored: the trace is observability, and losing a
    /// role marker must never fail a coding run.
    pub fn trace(&self, role: &str) {
        if let Ok(mut trace) = self.trace.lock() {
            trace.push(role.to_owned());
        }
    }

    /// Returns the roles this workspace saw execute, in order.
    #[must_use]
    pub fn roles(&self) -> Vec<String> {
        self.trace
            .lock()
            .map(|trace| trace.clone())
            .unwrap_or_default()
    }
}

/// Wall-clock bound on one test-command run unless overridden with
/// [`ProjectTestRunner::with_timeout`].
pub const DEFAULT_TEST_RUNNER_TIMEOUT: Duration = Duration::from_secs(120);

/// Per-stream (stdout, stderr) byte cap on one test-command run unless
/// overridden with [`ProjectTestRunner::with_max_output_bytes`].
pub const DEFAULT_TEST_RUNNER_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// A project test runner: the command whose exit status decides whether the
/// project's own tests pass.
///
/// Declared by [`code.test-gated-edit`](super::topology::TEST_GATED) and never
/// installed by the hermetic host path: a hermetic proof cannot shell out to
/// a project's test command. Its absence is what the mechanical
/// `resource.missing` exclusion reports. A caller that *does* install one —
/// a live host, or a test — gets a real command run, not a re-check of the
/// stated edits: [`judge`](Self::judge) reports [`Status::Issue`] on a
/// nonzero exit, a signal termination, a command that could not even start,
/// a run that outlives its timeout, or a run whose stdout or stderr outgrows
/// its output cap — every one of them means the project's own tests did not
/// pass (or could not be trusted to report), which is the only claim this
/// runner makes. The command itself is run directly, never through a shell.
///
/// The name says *project*, not one build tool. This identity is host-supplied
/// and reaches evidence records, so naming one language's runner here would
/// have put a benchmark's language into a stable identifier that outlives the
/// benchmark.
#[derive(Debug, Clone)]
pub struct ProjectTestRunner {
    program: String,
    args: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
}

impl ProjectTestRunner {
    /// Names the command whose exit status this runner judges by, bounded by
    /// [`DEFAULT_TEST_RUNNER_TIMEOUT`] and
    /// [`DEFAULT_TEST_RUNNER_MAX_OUTPUT_BYTES`] unless overridden.
    #[must_use]
    pub fn command(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            timeout: DEFAULT_TEST_RUNNER_TIMEOUT,
            max_output_bytes: DEFAULT_TEST_RUNNER_MAX_OUTPUT_BYTES,
        }
    }

    /// Overrides the wall-clock bound on the run.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Overrides the per-stream output cap.
    #[must_use]
    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    /// The command this runner judges by, without its arguments.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The command's arguments.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Runs the command in `working_dir` and reports only the verdict.
    ///
    /// A thin projection of [`run`](Self::run) for a caller that has no use
    /// for the captured output — every existing caller of this method.
    #[must_use]
    pub(super) fn judge(&self, working_dir: &std::path::Path) -> Status {
        self.run(working_dir).status
    }

    /// Runs the command in `working_dir` and reports the verdict plus a
    /// bounded tail of what it printed.
    ///
    /// [`Status::Pass`] only for a run that exits zero within the timeout with
    /// both streams under the output cap; everything else — a nonzero exit, a
    /// signal termination, a spawn failure, an exceeded timeout, or an
    /// exceeded output cap — is [`Status::Issue`]. A timeout or cap violation
    /// kills the child rather than waiting it out. The captured text is
    /// best-effort and independently bounded by
    /// [`MAX_CAPTURED_TEST_OUTPUT_BYTES`]: it may be empty even for a run that
    /// printed something, and it is never why a run is judged [`Status::Issue`]
    /// on its own.
    #[must_use]
    pub(super) fn run(&self, working_dir: &std::path::Path) -> TestRunOutcome {
        let mut child = match std::process::Command::new(&self.program)
            .args(&self.args)
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                return TestRunOutcome {
                    status: Status::Issue,
                    output: String::new(),
                    check_failure: None,
                };
            }
        };

        let cap_exceeded = Arc::new(AtomicBool::new(false));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let stdout_reader = child.stdout.take().map(|pipe| {
            spawn_capped_drain(
                pipe,
                self.max_output_bytes,
                Arc::clone(&cap_exceeded),
                Arc::clone(&captured),
                MAX_CAPTURED_TEST_OUTPUT_BYTES,
            )
        });
        let stderr_reader = child.stderr.take().map(|pipe| {
            spawn_capped_drain(
                pipe,
                self.max_output_bytes,
                Arc::clone(&cap_exceeded),
                Arc::clone(&captured),
                MAX_CAPTURED_TEST_OUTPUT_BYTES,
            )
        });

        let deadline = Instant::now() + self.timeout;
        let exit_status = loop {
            if cap_exceeded.load(Ordering::Relaxed) {
                break None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() >= deadline => break None,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break None,
            }
        };

        if exit_status.is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
        if let Some(reader) = stdout_reader {
            let _ = reader.join();
        }
        if let Some(reader) = stderr_reader {
            let _ = reader.join();
        }

        let status = match exit_status {
            Some(status) if !cap_exceeded.load(Ordering::Relaxed) && status.success() => {
                Status::Pass
            }
            _ => Status::Issue,
        };
        let output = captured
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        TestRunOutcome {
            status,
            output,
            check_failure: None,
        }
    }
}

impl LocalResource for ProjectTestRunner {}

/// Bound on the combined stdout/stderr text [`ProjectTestRunner::run`] keeps
/// for a repair step to read, independent of [`ProjectTestRunner::judge`]'s
/// own (much larger) pass/fail cap: a repair prompt needs enough of the
/// failure to act on, not the whole log.
const MAX_CAPTURED_TEST_OUTPUT_BYTES: usize = 4_096;

/// Cause of a failed optional self-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckFailureClass {
    /// The script or its runtime could not establish the requested behavior.
    Infrastructure,
    /// The check ran and reported a behavior failure.
    Behavior,
}

/// What one project test or optional self-check run produced: the verdict and
/// a bounded tail of its combined stdout/stderr that a repair step can read.
///
/// Distinct from [`Assessment<EditSite>`](polaris_cosmos::contracts::Assessment):
/// this is the raw material a repair step's model call reads to see *why* the
/// previous attempt failed, not the judged verdict a `Choose` predicate or
/// [`AssessmentConclusion`](polaris_cosmos::contracts::AssessmentConclusion)
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRunOutcome {
    /// The judged verdict, identical to what [`ProjectTestRunner::judge`]
    /// would have reported for the same run.
    pub status: Status,
    /// A bounded, best-effort tail of the run's combined stdout and stderr,
    /// empty when nothing was captured.
    pub output: String,
    /// The optional self-check's failure class; absent for passes and project tests.
    pub check_failure: Option<CheckFailureClass>,
}

/// Drains `pipe` on a dedicated thread, discarding bytes past `cap` and
/// setting `exceeded` the moment the total crosses it, so the reader never
/// blocks the child on a full OS pipe buffer while [`ProjectTestRunner::judge`]
/// polls for exit or timeout. Also copies up to `capture_cap` bytes into
/// `captured`, independent of `cap`, for a caller that wants to read the
/// output back rather than just know whether it stayed under the cap.
fn spawn_capped_drain(
    mut pipe: impl Read + Send + 'static,
    cap: usize,
    exceeded: Arc<AtomicBool>,
    captured: Arc<Mutex<Vec<u8>>>,
    capture_cap: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut total = 0usize;
        let mut buf = [0u8; 8192];
        loop {
            match pipe.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    total += read;
                    if total > cap {
                        exceeded.store(true, Ordering::Relaxed);
                    }
                    if let Ok(mut captured) = captured.lock() {
                        let remaining = capture_cap.saturating_sub(captured.len());
                        if remaining > 0 {
                            captured.extend_from_slice(&buf[..remaining.min(read)]);
                        }
                    }
                }
            }
        }
    })
}

/// The hand-built sweep's private worklist.
///
/// This exists because Cosmos's shipped [`coverage_sweep`] keeps its own
/// worklist and ledger in types it does not export, so no embodiment can
/// explain them to the admission boundary. An embodiment that wants an
/// admissible sweep has to own the state itself — as this does.
///
/// [`coverage_sweep`]: polaris_cosmos::cognition::approved::coverage_sweep
#[derive(Debug)]
pub struct SweepWorklist {
    pending: VecDeque<EditSite>,
    visited: Vec<EditSite>,
    budget: usize,
}

impl LocalResource for SweepWorklist {}

impl SweepWorklist {
    /// Creates an empty worklist with an iteration budget.
    #[must_use]
    pub const fn new(budget: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            visited: Vec::new(),
            budget,
        }
    }

    /// Replaces the queue with a freshly scanned site set.
    pub fn seed(&mut self, sites: Vec<EditSite>) {
        self.pending = sites.into();
        self.visited.clear();
    }

    /// Removes the next queued site.
    pub fn take_next(&mut self) -> Option<EditSite> {
        self.pending.pop_front()
    }

    /// Records an adjudicated site.
    pub fn record(&mut self, site: EditSite) {
        self.visited.push(site);
        self.budget = self.budget.saturating_sub(1);
    }

    /// Returns the sites adjudicated so far.
    #[must_use]
    pub fn visited(&self) -> &[EditSite] {
        &self.visited
    }

    /// Returns the sites never reached, which become visible residuals.
    #[must_use]
    pub fn unvisited(&self) -> Vec<EditSite> {
        self.pending.iter().cloned().collect()
    }

    /// Returns current progress.
    #[must_use]
    pub fn progress(&self) -> SweepProgress {
        SweepProgress {
            remaining: self.pending.len(),
            visited: self.visited.len(),
            budget_left: self.budget,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The answer shape
// ─────────────────────────────────────────────────────────────────────────────

/// One thing the answer topology found out about the working tree.
///
/// This is the second assessment subject this embodiment owns, and it exists
/// for a specific reason. The coding contract terminates in an
/// [`AssessmentConclusion`] over a non-empty ledger, and every existing
/// subject is an [`EditSite`] — a place something *should be written*. A
/// question that needed no edits has no edit site to report, and widening what
/// an edit site means to cover "I read this, and here is what it says" would
/// make the edit vocabulary lie. So a finding is its own subject, with its own
/// schema, and both shapes conclude through the one ordinary contract without
/// either borrowing the other's meaning.
///
/// [`AssessmentConclusion`]: polaris_cosmos::contracts::AssessmentConclusion
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QuestionFinding {
    /// The question this finding answers.
    pub question: String,
    /// Path the finding is about, relative to the sandbox root, or empty when
    /// the finding is about the tree as a whole.
    pub path: String,
    /// One-based line the finding is anchored to, or zero when it is not.
    pub line: usize,
    /// The finding itself, in one line.
    pub claim: String,
    /// [`Status::Pass`] for a grounded finding, [`Status::Indeterminate`] for
    /// "this question was not answerable here".
    pub status: Status,
}

impl QuestionFinding {
    /// Creates a finding.
    #[must_use]
    pub const fn new(
        question: String,
        path: String,
        line: usize,
        claim: String,
        status: Status,
    ) -> Self {
        Self {
            question,
            path,
            line,
            claim,
            status,
        }
    }
}

/// Prefixes [`CodingQuestion::parse`] recognises as a file inspection.
const INSPECT_PREFIXES: [&str; 4] = ["what is in ", "what's in ", "show me ", "cat "];

/// Prefixes [`CodingQuestion::parse`] recognises as a locate.
const LOCATE_PREFIXES: [&str; 5] = [
    "which files contain ",
    "which file contains ",
    "where is ",
    "where are ",
    "find ",
];

/// The question the answer topology was asked, in the closed set of shapes it
/// can actually serve.
///
/// The enum *is* the honesty guarantee. A question this embodiment cannot
/// answer does not become a best-effort attempt: it is carried as
/// [`Unsupported`](Self::Unsupported) all the way to a terminal conclusion
/// that says so, so a later routing decision can fall back rather than
/// present a shrug as a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodingQuestion {
    /// "Where in this tree does `needle` occur?" — answered by a literal,
    /// sandbox-rooted scan over the working tree.
    Locate {
        /// Literal text to look for. Not a regex, and not a symbol resolver.
        needle: String,
    },
    /// "What is in this file?" — answered by reading one sandbox-relative
    /// path and reporting its shape.
    Inspect {
        /// Sandbox-relative path to read.
        path: String,
    },
    /// A question outside both shapes, retained verbatim so the refusal can
    /// quote what was asked.
    Unsupported {
        /// The question as it arrived.
        text: String,
    },
}

impl LocalResource for CodingQuestion {}

impl CodingQuestion {
    /// Asks where a literal string occurs in the working tree.
    #[must_use]
    pub fn locate(needle: impl Into<String>) -> Self {
        let needle = needle.into();
        if needle.trim().is_empty() {
            return Self::Unsupported { text: needle };
        }
        Self::Locate { needle }
    }

    /// Asks what one sandbox-relative file contains.
    #[must_use]
    pub fn inspect(path: impl Into<String>) -> Self {
        let path = path.into();
        if path.trim().is_empty() {
            return Self::Unsupported { text: path };
        }
        Self::Inspect { path }
    }

    /// The question a task carries when it asked nothing.
    #[must_use]
    pub fn none() -> Self {
        Self::Unsupported {
            text: String::new(),
        }
    }

    /// Recognises a natural-language question, or refuses it.
    ///
    /// This is a deliberately small, documented set of phrasings over the two
    /// typed shapes above — a lexical front door, not an interpreter. It
    /// recognises `where is X`, `where are X`, `find X`, `which file(s)
    /// contain(s) X` as a [`Locate`](Self::Locate), and `what is in <path>`,
    /// `what's in <path>`, `show me <path>`, `cat <path>` as an
    /// [`Inspect`](Self::Inspect). Anything else is
    /// [`Unsupported`](Self::Unsupported) *by construction*: a phrasing this
    /// function guessed at is a phrasing the topology would answer badly.
    #[must_use]
    pub fn parse(text: impl Into<String>) -> Self {
        let text = text.into();
        // Matched case-insensitively, but the *original* slice is taken, so an
        // identifier or a path keeps its case. Every prefix is ASCII, so the
        // matched region has the same byte length in both forms.
        let original = text.trim().trim_end_matches('?').trim();
        let normalized = original.to_lowercase();
        for prefix in INSPECT_PREFIXES {
            if normalized.starts_with(prefix) {
                return Self::inspect(original[prefix.len()..].trim());
            }
        }
        for prefix in LOCATE_PREFIXES {
            if normalized.starts_with(prefix) {
                return Self::locate(original[prefix.len()..].trim());
            }
        }
        Self::Unsupported { text }
    }

    /// Whether this question is one of the shapes the topology can serve.
    #[must_use]
    pub const fn is_answerable(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }

    /// The question restated, for quoting back inside a finding.
    #[must_use]
    pub fn asked(&self) -> String {
        match self {
            Self::Locate { needle } => format!("where does `{needle}` occur"),
            Self::Inspect { path } => format!("what is in `{path}`"),
            Self::Unsupported { text } if text.trim().is_empty() => {
                "(no question was asked)".to_owned()
            }
            Self::Unsupported { text } => text.clone(),
        }
    }
}

#[cfg(test)]
mod test_runner_tests {
    use std::time::{Duration, Instant};

    use super::{ProjectTestRunner, Status};

    #[test]
    fn a_zero_exit_command_passes() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = ProjectTestRunner::command("true", Vec::<String>::new());
        assert_eq!(runner.judge(dir.path()), Status::Pass);
    }

    #[test]
    fn a_nonzero_exit_command_is_an_issue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = ProjectTestRunner::command("false", Vec::<String>::new());
        assert_eq!(runner.judge(dir.path()), Status::Issue);
    }

    #[test]
    fn a_program_that_cannot_even_start_is_an_issue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = ProjectTestRunner::command(
            "polaris-code-test-runner-program-does-not-exist",
            Vec::<String>::new(),
        );
        assert_eq!(runner.judge(dir.path()), Status::Issue);
    }

    /// A run that outlives its timeout is killed rather than waited out: the
    /// command sleeps for 5 seconds but the bound is 150ms, so a passing
    /// judgment well under that sleep proves the child was actually killed,
    /// not that it happened to finish.
    #[test]
    fn a_run_past_its_timeout_is_killed_and_reported_as_an_issue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = ProjectTestRunner::command("sleep", ["5".to_owned()])
            .with_timeout(Duration::from_millis(150));
        let started = Instant::now();
        assert_eq!(runner.judge(dir.path()), Status::Issue);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "the timeout should have killed the child long before its 5s sleep finished"
        );
    }

    /// A run whose output outgrows the cap is killed rather than waited out:
    /// `yes` never exits on its own, so a passing judgment well under the
    /// (generous) timeout proves the output cap — not the timeout — ended it.
    #[test]
    fn output_past_the_cap_is_killed_and_reported_as_an_issue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let runner = ProjectTestRunner::command("yes", Vec::<String>::new())
            .with_max_output_bytes(16)
            .with_timeout(Duration::from_secs(30));
        let started = Instant::now();
        assert_eq!(runner.judge(dir.path()), Status::Issue);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the output cap should have killed the child long before the 30s timeout"
        );
    }
}
