//! Durable, coding-owned tracked-edit rewind state.
//!
//! Conversation restoration stays on Polaris' checkpoint handle. This module
//! owns only exact file images written by `edit_file`; shell writes, external
//! edits, creates, deletes, build output, memory and remote side effects are
//! deliberately outside the boundary.

use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use polaris::sessions::checkpoint::{CheckpointId, CheckpointStatus, RevisionId};
use polaris::sessions::{SessionId, SessionsAPI};
use polaris_ontrace::oxigraph::OxigraphStore;
use polaris_tui::commands::CommandSpec;
use serde::{Deserialize, Serialize};

use crate::agents::{AgentConfig, ContextManager, LoopState, PolicySnapshot};
use crate::tools::{CodeIndexer, enrich};

use super::{AttachmentStore, BackgroundTasks, SessionSettings, TurnSteering};

const VERSION: u32 = 1;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_INVOCATIONS: usize = 50;
const MAX_IMAGE: usize = 8 * 1024 * 1024;

/// Which independent state axes a rewind requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RewindMode {
    /// Restore only native conversation resources.
    Conversation,
    /// Restore only safely tracked file edits.
    Edits,
    /// Restore files first, then conversation state.
    Both,
}

impl RewindMode {
    /// Parse the terminal spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "conversation" => Some(Self::Conversation),
            "edits" => Some(Self::Edits),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
    /// Whether conversation state is requested.
    pub fn conversation(self) -> bool {
        matches!(self, Self::Conversation | Self::Both)
    }
    /// Whether tracked edits are requested.
    pub fn edits(self) -> bool {
        matches!(self, Self::Edits | Self::Both)
    }
}

/// One selectable before-input boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindPoint {
    /// Stable journal-local display identity.
    pub id: u64,
    /// Native session identity.
    pub session: String,
    /// History revision on which this point was offered.
    pub revision: RevisionId,
    /// Native checkpoint identity.
    pub checkpoint: CheckpointId,
    /// Last edit sequence included by the boundary.
    pub edit_cursor: u64,
    /// Human-readable prompt boundary label.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Edit {
    sequence: u64,
    path: PathBuf,
    before: Vec<u8>,
    after: Vec<u8>,
    tracked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    version: u32,
    workspace: PathBuf,
    next: u64,
    floor: u64,
    edits: VecDeque<Edit>,
    points: Vec<RewindPoint>,
    recovery_required: Option<String>,
}

/// A per-workspace durable journal shared by the host and `edit_file`.
#[derive(Clone, Debug)]
pub struct EditJournal {
    path: PathBuf,
    inner: Arc<Mutex<State>>,
}

/// One path's tracked-edit chain inside a journal window.
///
/// Read-only: produced by [`EditJournal::tracked_since`] for review surfaces
/// that need the images but must not restore them.
#[derive(Debug, Clone)]
pub struct TrackedPath {
    /// Absolute path the chain wrote to.
    pub path: PathBuf,
    /// Bytes the first tracked edit in the window replaced.
    pub before: Vec<u8>,
    /// Bytes the last tracked edit in the window wrote.
    pub after: Vec<u8>,
    /// How many `edit_file` writes the window holds for this path.
    pub edits: usize,
    /// Whether every image in the chain was small enough to retain.
    pub images_complete: bool,
}

/// A read-only view of the tracked edits recorded after a journal cursor.
#[derive(Debug, Clone, Default)]
pub struct TrackedWindow {
    /// Per-path chains, ordered by path.
    pub paths: Vec<TrackedPath>,
    /// Whether retention evicted edits the window would have contained.
    pub truncated: bool,
}

/// Per-path result of restoring tracked edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathOutcome {
    /// The target bytes replaced the tracked post-image.
    Restored(PathBuf),
    /// The file already contained the target bytes.
    AlreadyRestored(PathBuf),
    /// Safety validation refused this path without mutation.
    Skipped(PathBuf, String),
    /// A prepared replacement failed; later paths were not attempted by the caller.
    Failed(PathBuf, String),
}

/// A confirmed host action, consumed only after the input-held turn exits.
#[derive(Debug, Clone)]
pub struct RewindRequest {
    /// Selected point.
    pub point: RewindPoint,
    /// Requested axes.
    pub mode: RewindMode,
}

/// The production resources needed to apply a confirmed rewind.
///
/// This is deliberately a narrow host seam rather than a second path into the
/// graph. Conversation state still moves only through the native checkpoint
/// handle and `ContextManager` is reached only while holding the session's
/// context lock. The host uses `restore_exact`: a serializer mismatch is a
/// refusal before mutation, never the permissive partial-restore state that
/// makes a session reject later turns.
#[derive(Clone)]
pub struct RewindApplication {
    sessions: SessionsAPI,
    journal: EditJournal,
    config: AgentConfig,
    settings: SessionSettings,
    steering: TurnSteering,
    attachments: AttachmentStore,
    tasks: BackgroundTasks,
    policy: PolicySnapshot,
    store: Option<OxigraphStore>,
    working_dir: PathBuf,
}

/// Observable result of applying both independent rewind axes.
#[derive(Debug)]
pub struct RewindApplicationOutcome {
    /// Per-path exact-image outcomes.
    pub files: Vec<PathOutcome>,
    /// Conversation-axis detail.
    pub conversation: String,
    /// Index-axis detail.
    pub index: String,
    /// Native persistence result.
    pub save: Result<(), String>,
}

impl RewindApplication {
    /// Bind the same resources the interactive composition keeps between turns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sessions: SessionsAPI,
        journal: EditJournal,
        config: AgentConfig,
        settings: SessionSettings,
        steering: TurnSteering,
        attachments: AttachmentStore,
        tasks: BackgroundTasks,
        policy: PolicySnapshot,
        store: Option<OxigraphStore>,
        working_dir: PathBuf,
    ) -> Self {
        Self {
            sessions,
            journal,
            config,
            settings,
            steering,
            attachments,
            tasks,
            policy,
            store,
            working_dir,
        }
    }

    /// Apply a confirmed request after the driving turn has released its lock.
    ///
    /// Exact checkpoint restoration captures the live state first, preserving
    /// the route back to it. A hard file-write failure stops the conversation
    /// axis; already-restored file images remain idempotent, so repairing the
    /// failing path and retrying is the approved partial-failure recovery.
    pub async fn apply(
        &self,
        session: &SessionId,
        request: &RewindRequest,
    ) -> RewindApplicationOutcome {
        // Wait behind an in-flight graph turn before touching either axis.
        // `main` calls this after `drive_turn` returns; this explicit barrier
        // also makes that quiescence contract load-bearing at this seam.
        let _ = self.sessions.with_context(session, |_| ()).await;
        let paths = self.journal.affected_paths(&request.point);
        let files = if request.mode.edits() {
            self.journal.restore_edits(&request.point)
        } else {
            Vec::new()
        };
        let hard_file_failure = files
            .iter()
            .any(|outcome| matches!(outcome, PathOutcome::Failed(_, _)));
        let mut restored_conversation = false;
        let conversation = if request.mode.conversation() && !hard_file_failure {
            match self
                .sessions
                .checkpoints()
                .restore_exact(session, request.point.checkpoint)
                .await
            {
                Ok(restored) => {
                    if !restored.carried_over.is_empty()
                        && let Err(error) = self.sessions.setup_session(session).await
                    {
                        format!("failed during required setup: {error}")
                    } else {
                        restored_conversation = true;
                        "restored exactly".to_string()
                    }
                }
                Err(error) => format!("refused/failed: {error}"),
            }
        } else if hard_file_failure {
            "not attempted after a hard file-write failure".to_string()
        } else {
            "not requested".to_string()
        };

        if restored_conversation {
            let cfg = self.config.clone();
            let settings = self.settings.clone();
            let steering = self.steering.clone();
            let attachments = self.attachments.clone();
            let tasks = self.tasks.clone();
            let policy = self.policy.clone();
            let _ = self
                .sessions
                .with_context(session, move |ctx| {
                    if let Ok(mut history) = ctx.get_resource_mut::<ContextManager>() {
                        history.reset_runtime(cfg.prompt_cache);
                    }
                    ctx.insert(LoopState::default());
                    ctx.insert(cfg);
                    ctx.insert(settings);
                    ctx.insert(steering);
                    ctx.insert(attachments);
                    ctx.insert(tasks);
                    ctx.insert(policy);
                })
                .await;
        }

        let index = if paths.is_empty() {
            "not needed".to_string()
        } else if let Some(store) = &self.store {
            match CodeIndexer::index_paths(store, &self.working_dir, &paths)
                .await
                .and_then(|_| enrich(store))
            {
                Ok(()) => "refreshed".to_string(),
                Err(error) => format!("stale/unavailable: {error}"),
            }
        } else {
            "stale/unavailable: code store missing".to_string()
        };
        let save = self
            .sessions
            .save_session(session)
            .await
            .map_err(|error| error.to_string());
        RewindApplicationOutcome {
            files,
            conversation,
            index,
            save,
        }
    }

    /// Render the generic TUI report without moving coding policy into TUI.
    pub fn report(
        request: &RewindRequest,
        outcome: &RewindApplicationOutcome,
    ) -> polaris_tui::commands::RewindReport {
        let hard_file_failure = outcome
            .files
            .iter()
            .any(|item| matches!(item, PathOutcome::Failed(_, _)));
        let files = outcome
            .files
            .iter()
            .map(|item| format!("- {item:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file_status = if hard_file_failure {
            "failed"
        } else if outcome
            .files
            .iter()
            .any(|item| matches!(item, PathOutcome::Skipped(_, _)))
        {
            "partial"
        } else if request.mode.edits() {
            "restored"
        } else {
            "not-requested"
        };
        polaris_tui::commands::RewindReport::new(
            request.point.id.to_string(),
            format!("{:?}", request.mode).to_lowercase(),
        )
        .with_axis(
            "files",
            file_status,
            if files.is_empty() { "none" } else { &files },
        )
        .with_axis(
            "conversation",
            if outcome.conversation == "restored exactly" {
                "restored"
            } else {
                "refused/not-requested"
            },
            &outcome.conversation,
        )
        .with_axis(
            "index",
            if outcome.index == "refreshed" || outcome.index == "not needed" {
                "fresh"
            } else {
                "stale"
            },
            &outcome.index,
        )
        .with_axis(
            "persistence",
            if outcome.save.is_ok() {
                "saved"
            } else {
                "failed"
            },
            outcome
                .save
                .as_ref()
                .map(|_| "native session and checkpoints saved")
                .unwrap_or("original checkpoint recovery route retained"),
        )
        .excluding("shell commands")
        .excluding("external edits")
        .excluding("file creation/deletion")
        .excluding("build artifacts")
        .excluding("memory and remote side effects")
    }
}

/// Command-facing rewind state. It queues work but never restores while the
/// graph holds the session context.
#[derive(Clone)]
pub struct RewindControl {
    sessions: SessionsAPI,
    journal: EditJournal,
    current: Arc<dyn Fn() -> SessionId + Send + Sync>,
    pending: Arc<Mutex<Option<RewindRequest>>>,
}

impl std::fmt::Debug for RewindControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RewindControl").finish_non_exhaustive()
    }
}

impl RewindControl {
    /// Bind native checkpoints, the coding journal and current-session lookup.
    pub fn new(
        sessions: SessionsAPI,
        journal: EditJournal,
        current: impl Fn() -> SessionId + Send + Sync + 'static,
    ) -> Self {
        Self {
            sessions,
            journal,
            current: Arc::new(current),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// Register `/rewind`. The first invocation previews; repeating it with
    /// `confirm` queues the action and releases the idle turn.
    pub fn register(&self, registry: &super::CommandRegistry) -> Result<(), super::CommandError> {
        let this = self.clone();
        registry.register(
            CommandSpec::new(
                "rewind",
                "Preview or confirm conversation/tracked-edit rewind",
            )
            .with_usage("[point] [conversation|edits|both] [confirm]"),
            Arc::new(move |ctx| {
                let this = this.clone();
                let args = ctx.args.to_string();
                Box::pin(async move { this.command(&args) })
            }),
        )
    }

    fn command(&self, args: &str) -> super::CommandOutcome {
        let session = (self.current)();
        let checkpoints = self.sessions.checkpoints();
        let revision = match checkpoints.revision(&session) {
            Ok(value) => value,
            Err(error) => {
                return super::CommandOutcome::Error(format!("rewind unavailable: {error}"));
            }
        };
        let parts: Vec<_> = args.split_whitespace().collect();
        if parts.is_empty() {
            let points = self.journal.points(&session, revision);
            if points.is_empty() {
                return super::CommandOutcome::Reply(
                    "No rewind points are retained for the current history.".into(),
                );
            }
            let rows = points
                .into_iter()
                .map(|p| {
                    format!(
                        "{}: {} (checkpoint {}, {} tracked path(s))",
                        p.id,
                        p.label,
                        p.checkpoint,
                        self.journal.affected_paths(&p).len()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            return super::CommandOutcome::Reply(format!(
                "Rewind targets (newest last):\n{rows}\n\nChanges outside `edit_file` tracking are never restored."
            ));
        }
        let Ok(id) = parts[0].parse::<u64>() else {
            return super::CommandOutcome::Error(
                "usage: /rewind [point] [conversation|edits|both] [confirm]".into(),
            );
        };
        let mode = parts
            .get(1)
            .and_then(|v| RewindMode::parse(v))
            .unwrap_or(RewindMode::Both);
        let Some(point) = self.journal.point(id) else {
            return super::CommandOutcome::Error(format!(
                "rewind point {id} was never recorded or was evicted"
            ));
        };
        if point.session != session.as_str() || point.revision != revision {
            return super::CommandOutcome::Error(
                "that point belongs to another session or abandoned history revision".into(),
            );
        }
        if mode.conversation() {
            match checkpoints.status(&session, point.checkpoint) {
                Ok(CheckpointStatus::Evicted { .. }) => return super::CommandOutcome::Error("the conversation checkpoint was evicted; choose edits-only explicitly if its file images remain".into()),
                Ok(CheckpointStatus::Unknown(_)) => return super::CommandOutcome::Error("the conversation checkpoint never existed in this session".into()),
                Err(error) => return super::CommandOutcome::Error(format!("checkpoint unavailable: {error}")),
                _ => {}
            }
        }
        let paths = self.journal.affected_paths(&point);
        if mode.edits() && !self.journal.edits_available(&point) {
            return super::CommandOutcome::Error(
                "tracked file images for that point were evicted; choose conversation-only explicitly"
                    .into(),
            );
        }
        let path_text = if paths.is_empty() {
            "(no tracked paths)".into()
        } else {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        if parts.get(2).copied() != Some("confirm") {
            return super::CommandOutcome::Reply(format!(
                "Preview point {id}, mode {mode:?}:\n{path_text}\n\nExternal changes conflict and are skipped. Shell/external/create/delete/build/memory effects are outside tracking. Ensure other writers are quiescent, then run `/rewind {id} {} confirm`.",
                match mode {
                    RewindMode::Conversation => "conversation",
                    RewindMode::Edits => "edits",
                    RewindMode::Both => "both",
                }
            ));
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        if pending.is_some() {
            return super::CommandOutcome::Error("another rewind is already pending".into());
        }
        *pending = Some(RewindRequest { point, mode });
        super::CommandOutcome::Control(format!(
            "Rewind {id} confirmed; applying at the quiescent host boundary."
        ))
    }

    /// Take the one confirmed request.
    pub fn take_pending(&self) -> Option<RewindRequest> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl EditJournal {
    /// Open or create `<workspace>/.polaris/rewind-v1.json`.
    ///
    /// The journal is agent bookkeeping, so it goes into the workspace's
    /// [`state_dir`](crate::state::state_dir), which git ignores.
    pub fn open(workspace: &Path) -> Result<Self, String> {
        let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
        let path = crate::state::state_dir(&workspace)
            .map_err(|e| e.to_string())?
            .join("rewind-v1.json");
        let state = if path.exists() {
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            let state: State = serde_json::from_slice(&bytes)
                .map_err(|e| format!("rewind journal is corrupt: {e}"))?;
            if state.version != VERSION || state.workspace != workspace {
                return Err("rewind journal belongs to a different workspace or version".into());
            }
            state
        } else {
            State {
                version: VERSION,
                workspace,
                next: 1,
                floor: 0,
                edits: VecDeque::new(),
                points: Vec::new(),
                recovery_required: None,
            }
        };
        let journal = Self {
            path,
            inner: Arc::new(Mutex::new(state)),
        };
        journal.persist()?;
        Ok(journal)
    }

    fn persist(&self) -> Result<(), String> {
        let state = self.inner.lock().map_err(|e| e.to_string())?;
        let bytes = serde_json::to_vec(&*state).map_err(|e| e.to_string())?;
        drop(state);
        if let Some(parent) = self.path.parent() {
            // Re-ensure rather than plain `create_dir_all`: a state directory
            // removed under a live run must come back with its marker, or the
            // journal reappears in the next `git add -A`.
            crate::state::ensure_ignored(parent).map_err(|e| e.to_string())?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Record a successful whole-file edit. Both images are exact bytes.
    pub fn record_edit(&self, path: &Path, before: Vec<u8>, after: Vec<u8>) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|e| e.to_string())?;
        if let Some(reason) = &state.recovery_required {
            return Err(format!("rewind recovery required: {reason}"));
        }
        let sequence = state.next;
        state.next += 1;
        let tracked = before.len() <= MAX_IMAGE && after.len() <= MAX_IMAGE;
        state.edits.push_back(Edit {
            sequence,
            path: path.to_path_buf(),
            before: if tracked { before } else { Vec::new() },
            after: if tracked { after } else { Vec::new() },
            tracked,
        });
        while state.edits.len() > MAX_INVOCATIONS || journal_bytes(&state) > MAX_BYTES {
            let Some(old) = state.edits.pop_front() else {
                break;
            };
            state.floor = old.sequence;
        }
        drop(state);
        self.persist()
    }

    /// Add a boundary that couples a native checkpoint to the current edit cursor.
    pub fn add_point(
        &self,
        session: &SessionId,
        revision: RevisionId,
        checkpoint: CheckpointId,
        label: String,
    ) -> Result<RewindPoint, String> {
        let mut state = self.inner.lock().map_err(|e| e.to_string())?;
        let point = RewindPoint {
            id: state.points.last().map_or(1, |p| p.id + 1),
            session: session.as_str().to_string(),
            revision,
            checkpoint,
            edit_cursor: state.next.saturating_sub(1),
            label,
        };
        state.points.push(point.clone());
        if state.points.len() > MAX_INVOCATIONS {
            state.points.remove(0);
        }
        drop(state);
        self.persist()?;
        Ok(point)
    }

    /// Current-history points for a session.
    pub fn points(&self, session: &SessionId, revision: RevisionId) -> Vec<RewindPoint> {
        self.inner
            .lock()
            .map(|s| {
                s.points
                    .iter()
                    .filter(|p| p.session == session.as_str() && p.revision == revision)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Find any retained point by display id.
    pub fn point(&self, id: u64) -> Option<RewindPoint> {
        self.inner
            .lock()
            .ok()?
            .points
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    /// Preview affected paths, without claiming they remain safe at commit.
    pub fn affected_paths(&self, point: &RewindPoint) -> Vec<PathBuf> {
        let Ok(state) = self.inner.lock() else {
            return Vec::new();
        };
        let mut paths: Vec<_> = state
            .edits
            .iter()
            .filter(|e| e.sequence > point.edit_cursor)
            .map(|e| e.path.clone())
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    /// Whether every image after this cursor is still retained.
    pub fn edits_available(&self, point: &RewindPoint) -> bool {
        self.inner
            .lock()
            .map(|state| point.edit_cursor >= state.floor)
            .unwrap_or(false)
    }

    /// The canonical workspace this journal belongs to.
    pub fn workspace(&self) -> PathBuf {
        self.inner
            .lock()
            .map(|state| state.workspace.clone())
            .unwrap_or_default()
    }

    /// The edit cursor of the earliest retained boundary for `session`.
    ///
    /// A boundary is captured before every prompt, including the session's
    /// first, so this is where the session started as far as the journal is
    /// concerned. `None` means no boundary for that session survived
    /// retention (or none was ever captured, as in `--print`).
    pub fn session_start(&self, session: &SessionId) -> Option<u64> {
        self.inner
            .lock()
            .ok()?
            .points
            .iter()
            .filter(|point| point.session == session.as_str())
            .map(|point| point.edit_cursor)
            .min()
    }

    /// Group the tracked edits after `cursor` by path, without restoring any.
    ///
    /// Purely a read: nothing on disk, in the journal or in the session is
    /// touched.
    pub fn tracked_since(&self, cursor: u64) -> TrackedWindow {
        let Ok(state) = self.inner.lock() else {
            return TrackedWindow::default();
        };
        let mut grouped: BTreeMap<PathBuf, Vec<&Edit>> = BTreeMap::new();
        for edit in state.edits.iter().filter(|e| e.sequence > cursor) {
            grouped.entry(edit.path.clone()).or_default().push(edit);
        }
        let paths = grouped
            .into_iter()
            .map(|(path, chain)| {
                let first = chain.first().expect("grouped chains are non-empty");
                let last = chain.last().expect("grouped chains are non-empty");
                TrackedPath {
                    path,
                    before: first.before.clone(),
                    after: last.after.clone(),
                    edits: chain.len(),
                    images_complete: chain.iter().all(|edit| edit.tracked),
                }
            })
            .collect();
        TrackedWindow {
            paths,
            truncated: cursor < state.floor,
        }
    }

    /// Restore existing regular single-link files whose complete tracked chain still matches.
    pub fn restore_edits(&self, point: &RewindPoint) -> Vec<PathOutcome> {
        let Ok(state) = self.inner.lock() else {
            return vec![PathOutcome::Failed(
                PathBuf::new(),
                "journal lock poisoned".into(),
            )];
        };
        let mut grouped: BTreeMap<PathBuf, Vec<Edit>> = BTreeMap::new();
        for edit in state
            .edits
            .iter()
            .filter(|e| e.sequence > point.edit_cursor)
        {
            grouped
                .entry(edit.path.clone())
                .or_default()
                .push(edit.clone());
        }
        let workspace = state.workspace.clone();
        drop(state);
        if !self.edits_available(point) {
            return vec![PathOutcome::Skipped(
                PathBuf::new(),
                "required file images were evicted by journal retention".into(),
            )];
        }
        let mut outcomes = Vec::new();
        for (path, chain) in grouped {
            let outcome = restore_path(&workspace, path, &chain);
            let failed = matches!(outcome, PathOutcome::Failed(_, _));
            outcomes.push(outcome);
            if failed {
                break;
            }
        }
        outcomes
    }
}

fn journal_bytes(state: &State) -> usize {
    state
        .edits
        .iter()
        .map(|e| e.before.len() + e.after.len() + e.path.as_os_str().len() + 128)
        .sum()
}

fn restore_path(workspace: &Path, path: PathBuf, chain: &[Edit]) -> PathOutcome {
    if !path.starts_with(workspace) || chain.iter().any(|e| !e.tracked) {
        return PathOutcome::Skipped(path, "image unavailable or outside workspace".into());
    }
    for pair in chain.windows(2) {
        if pair[0].after != pair[1].before {
            return PathOutcome::Skipped(
                path,
                "tracked chain contains an external-edit gap".into(),
            );
        }
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(v) => v,
        Err(e) => return PathOutcome::Skipped(path, format!("file unavailable: {e}")),
    };
    if !metadata.file_type().is_file() || metadata.nlink() > 1 {
        return PathOutcome::Skipped(path, "unsafe non-regular or multiply-linked file".into());
    }
    let current = match fs::read(&path) {
        Ok(v) => v,
        Err(e) => return PathOutcome::Skipped(path, e.to_string()),
    };
    let target = &chain[0].before;
    let expected = &chain.last().expect("nonempty chain").after;
    if current == *target {
        return PathOutcome::AlreadyRestored(path);
    }
    if current != *expected {
        return PathOutcome::Skipped(path, "file changed outside tracked edits".into());
    }
    let tmp = path.with_extension(format!("polaris-rewind-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        fs::write(&tmp, target)?;
        fs::set_permissions(&tmp, metadata.permissions())?;
        OpenOptions::new().read(true).open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(())
    })();
    match result {
        Ok(()) => PathOutcome::Restored(path),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            PathOutcome::Failed(path, e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(cursor: u64) -> RewindPoint {
        RewindPoint {
            id: 1,
            session: "s".into(),
            revision: RevisionId::default(),
            checkpoint: serde_json::from_str("1").unwrap(),
            edit_cursor: cursor,
            label: "x".into(),
        }
    }

    #[test]
    fn skips_external_changes_and_restores_exact_chain() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, b"a").unwrap();
        let j = EditJournal::open(dir.path()).unwrap();
        j.record_edit(&file, b"a".to_vec(), b"b".to_vec()).unwrap();
        fs::write(&file, b"b").unwrap();
        let point = point(0);
        assert!(matches!(
            j.restore_edits(&point)[0],
            PathOutcome::Restored(_)
        ));
        assert_eq!(fs::read(&file).unwrap(), b"a");
        fs::write(&file, b"external").unwrap();
        assert!(matches!(
            j.restore_edits(&point)[0],
            PathOutcome::Skipped(_, _)
        ));
    }

    #[test]
    fn restart_keeps_images_and_second_rewind_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, b"one").unwrap();
        let journal = EditJournal::open(dir.path()).unwrap();
        journal
            .record_edit(&file, b"one".to_vec(), b"two".to_vec())
            .unwrap();
        fs::write(&file, b"two").unwrap();
        drop(journal);
        let resumed = EditJournal::open(dir.path()).unwrap();
        assert!(matches!(
            resumed.restore_edits(&point(0))[0],
            PathOutcome::Restored(_)
        ));
        assert!(matches!(
            resumed.restore_edits(&point(0))[0],
            PathOutcome::AlreadyRestored(_)
        ));
    }

    #[test]
    fn intermediate_external_edit_breaks_chain_even_when_tip_matches() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        fs::write(&file, b"a").unwrap();
        let journal = EditJournal::open(dir.path()).unwrap();
        journal
            .record_edit(&file, b"a".to_vec(), b"b".to_vec())
            .unwrap();
        journal
            .record_edit(&file, b"external".to_vec(), b"c".to_vec())
            .unwrap();
        fs::write(&file, b"c").unwrap();
        assert!(matches!(
            journal.restore_edits(&point(0))[0],
            PathOutcome::Skipped(_, _)
        ));
        assert_eq!(fs::read(&file).unwrap(), b"c");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_targets_are_never_mutated() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::write(&real, b"b").unwrap();
        symlink(&real, &link).unwrap();
        let journal = EditJournal::open(dir.path()).unwrap();
        journal
            .record_edit(&link, b"a".to_vec(), b"b".to_vec())
            .unwrap();
        assert!(matches!(
            journal.restore_edits(&point(0))[0],
            PathOutcome::Skipped(_, _)
        ));
        fs::remove_file(&link).unwrap();
        fs::hard_link(&real, &link).unwrap();
        let journal = EditJournal::open(dir.path()).unwrap();
        journal
            .record_edit(&link, b"a".to_vec(), b"b".to_vec())
            .unwrap();
        assert!(matches!(
            journal.restore_edits(&point(1))[0],
            PathOutcome::Skipped(_, _)
        ));
    }
}
