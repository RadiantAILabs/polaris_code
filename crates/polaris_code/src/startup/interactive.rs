//! The interactive turn driver: one turn per submission, and everything the
//! terminal front end does between turns.

use std::sync::{Arc, OnceLock};

use polaris::graph::GraphExecutor;
use polaris::plugins::IOMessage;
use polaris::sessions::checkpoint::CheckpointOrigin;
use polaris::sessions::{AgentTypeId, SessionsAPI};

use super::posture::policy_snapshot;
use crate::agents::{AgentConfig, PermissionMode};
use crate::run;
use crate::terminal::{
    self, AttachmentStore, BackgroundTasks, EditJournal, RewindApplication, RewindControl,
    SessionAction, SessionNavigation, SessionSettings, TerminalOutput,
};
use crate::tools::GraphSearchMode;

/// How much of the spend ceiling an interactive run may use before it says so
/// in the transcript — the "you are approaching the limit" point, warned once.
const CEILING_WARN_FRACTION: f64 = 0.8;

/// What the turn-driver task owns: the handles the loop reads between turns.
pub(super) struct Driver {
    /// The session API the turns run on.
    pub(super) sessions: SessionsAPI,
    /// The status sink the cancellation path emits through, set once the
    /// front end exists.
    pub(super) status: Arc<OnceLock<run::StatusEmitter>>,
    /// The canonical transcript the conversation panel browses.
    pub(super) history: Arc<terminal::history::CanonicalHistory>,
    /// Which session is current, and any switch waiting for a turn boundary.
    pub(super) navigation: SessionNavigation,
    /// Where the driver writes notices the conversation shows.
    pub(super) output: TerminalOutput,
    /// The configuration each session is created with.
    pub(super) config: AgentConfig,
    /// The live posture and model settings.
    pub(super) settings: SessionSettings,
    /// The mid-turn input queue.
    pub(super) steering: terminal::TurnSteering,
    /// Composer attachments.
    pub(super) attachments: AttachmentStore,
    /// Managed shell runs pushed out of a turn.
    pub(super) tasks: BackgroundTasks,
    /// The pending-rewind request.
    pub(super) rewind: RewindControl,
    /// The journal rewind points are recorded in.
    pub(super) journal: EditJournal,
    /// What applies a rewind request between turns.
    pub(super) rewind_application: RewindApplication,
    /// The agent type new sessions are created as.
    pub(super) agent_type: AgentTypeId,
    /// Which graph-search surface the sessions' policy withholds from.
    pub(super) graph_search: GraphSearchMode,
    /// The agent loop's per-turn iteration limit.
    pub(super) max_turns: usize,
}

impl Driver {
    /// Spawn the loop. It runs one agent turn per user submission, and ends
    /// when the session stops taking turns or the process exits.
    pub(super) fn spawn(self) -> tokio::task::JoinHandle<()> {
        let Self {
            sessions: driver_sessions,
            status: driver_status,
            history: driver_history,
            navigation: driver_navigation,
            output: driver_output,
            config: driver_config,
            settings: driver_settings,
            steering: driver_steering,
            attachments: driver_attachments,
            tasks: driver_tasks,
            rewind: driver_rewind,
            journal: driver_journal,
            rewind_application,
            agent_type: driver_agent_type,
            graph_search,
            max_turns,
        } = self;
        let executor = move || GraphExecutor::new().with_default_max_iterations(max_turns);
        tokio::spawn(async move {
            // One-shot latch for the "approaching the ceiling" notice below.
            let mut ceiling_warned = false;
            // `run::drive_until`'s loop, reproduced here only so the canonical
            // history the conversation panel browses is refreshed after each turn
            // — including a failed one, whose partial transcript stays browsable.
            // The durability point, the failure classification and the ceiling
            // are `drive_until`'s, called rather than restated.
            let mut budget = run::FailureBudget::new();
            loop {
                let driver_id = driver_navigation.current();
                // A rewind point is a before-input boundary. Capturing here happens
                // before the graph accepts the next prompt, including the first.
                match driver_sessions
                    .checkpoints()
                    .capture(&driver_id, CheckpointOrigin::Manual)
                    .await
                {
                    Ok(checkpoint) => {
                        if let Ok(revision) = driver_sessions.checkpoints().revision(&driver_id) {
                            let _ = driver_journal.add_point(
                                &driver_id,
                                revision,
                                checkpoint.id,
                                format!("before turn {}", checkpoint.resume_turn),
                            );
                        }
                    }
                    Err(error) => tracing::warn!("rewind boundary unavailable: {error}"),
                }
                let (outcome, step) =
                    run::drive_turn_step(&driver_sessions, &driver_id, driver_status.get()).await;
                if let Err(err) = driver_history.refresh(&driver_sessions, &driver_id).await {
                    tracing::warn!("canonical transcript history unavailable: {err}");
                }
                // A turn that answered submitted coding work owes its submitter
                // which of the three answers came back — a topology ran, none was
                // eligible, or none started. Display-only, like the rewind report
                // below: the canonical transcript belongs to the conversation, and
                // a topology turn never wrote to it.
                if let Some(report) = terminal::topology::turn_report(&outcome) {
                    let _ = driver_output.send(report).await;
                }
                // The durability point. A checkpoint capture is in-memory, so
                // until this call returns, everything since launch lives only in
                // this process. It runs between turns, where the session's
                // context lock is free, and it names `driver_id` — the session
                // this iteration actually drove, which after a `/new` or
                // `/resume` is not the one the process launched with.
                if let Err(message) = run::persist_turn(&driver_sessions, &driver_id).await {
                    tracing::error!("{message}");
                    let _ = driver_output
                        .send(
                            IOMessage::system_text(message)
                                .with_metadata(polaris::plugins::IO_TYPE, "error"),
                        )
                        .await;
                }
                // Whatever ended the turn — completion, failure, or the user's
                // Ctrl-C — a task that outlived it must not become invisible. The
                // count follows the turn's own last row, including `cancelled`,
                // which is exactly where "I interrupted it and it kept running" is
                // otherwise correct but surprising.
                let _ = driver_output
                    .send(polaris_tui::commands::report_status(
                        &polaris_tui::commands::StatusReport::default()
                            .with_tasks_running(driver_tasks.running()),
                    ))
                    .await;
                if let Some(request) = driver_rewind.take_pending() {
                    let outcome = rewind_application.apply(&driver_id, &request).await;
                    let report = RewindApplication::report(&request, &outcome);
                    let _ = driver_output.send(report.into_message()).await;
                    let _ = driver_history.refresh(&driver_sessions, &driver_id).await;
                }
                if let Some(action) = driver_navigation.take_pending() {
                    let target = match &action {
                        SessionAction::New(id) | SessionAction::Resume(id) => id.clone(),
                    };
                    if let Err(err) = driver_sessions.save_session(&driver_id).await {
                        let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session switch cancelled: failed to save `{}`: {err}. Fix storage and retry.",
                            driver_id.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                        continue;
                    }
                    let transition = match action {
                        SessionAction::Resume(_) => {
                            driver_sessions
                                .resume_session_with_executor(
                                    driver_sessions.create_context(),
                                    &target,
                                    executor(),
                                    {
                                        let cfg = driver_config.clone();
                                        let settings = driver_settings.clone();
                                        let steering = driver_steering.clone();
                                        let attachments = driver_attachments.clone();
                                        let tasks = driver_tasks.clone();
                                        move |ctx| {
                                            ctx.insert(cfg.clone());
                                            ctx.insert(settings.clone());
                                            ctx.insert(steering.clone());
                                            ctx.insert(attachments.clone());
                                            ctx.insert(tasks.clone());
                                        }
                                    },
                                )
                                .await
                        }
                        SessionAction::New(_) => driver_sessions.create_session_with_executor(
                            driver_sessions.create_context(),
                            &target,
                            &driver_agent_type,
                            executor(),
                            {
                                let cfg = driver_config.clone();
                                let settings = driver_settings.clone();
                                let steering = driver_steering.clone();
                                let attachments = driver_attachments.clone();
                                let tasks = driver_tasks.clone();
                                move |ctx| {
                                    ctx.insert(cfg.clone());
                                    ctx.insert(settings.clone());
                                    ctx.insert(steering.clone());
                                    ctx.insert(attachments.clone());
                                    ctx.insert(tasks.clone());
                                }
                            },
                        ),
                    };
                    if let Err(err) = transition {
                        let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session switch failed for `{}`: {err}. The previous session remains active; fix the id or storage and retry.",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                        continue;
                    }
                    // Modes are deliberately not persisted. Every session entered
                    // through `/new` or `/resume` starts in fail-closed Confirm,
                    // regardless of the posture of the session being left. The model
                    // is a preference rather than a posture, so it stays on whatever
                    // `/model` last chose.
                    // Confirm never nudges, so the nudge count does not matter.
                    let fresh_policy =
                        policy_snapshot(&driver_config, &PermissionMode::Confirm, graph_search, 0);
                    driver_settings.reset_keeping_model();
                    if let Err(err) = driver_sessions
                        .with_context(&target, move |ctx| {
                            ctx.insert(fresh_policy);
                        })
                        .await
                    {
                        let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session `{}` opened but its default permission mode could not be applied: {err}",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                        continue;
                    }
                    if let Err(err) = driver_history.refresh(&driver_sessions, &target).await {
                        let _ = driver_output
                        .send(IOMessage::system_text(format!(
                            "Session `{}` resumed but its canonical transcript could not be replayed: {err}",
                            target.as_str()
                        )).with_metadata(polaris::plugins::IO_TYPE, "error"))
                        .await;
                        continue;
                    }
                    let _ = driver_output
                        .send(polaris_tui::commands::session_replay_start(target.as_str()))
                        .await;
                    if let Ok(messages) = driver_history.replay() {
                        for message in messages {
                            let _ = driver_output.send(message).await;
                        }
                    }
                    let _ = driver_output
                        .send(polaris_tui::commands::session_replay_end(target.as_str()))
                        .await;
                    driver_navigation.set_current(target);
                    continue;
                }
                // The spend ceiling, where a human can see it. The live cost row
                // already shows what the run has spent; this adds the two things
                // the row cannot say — that a limit exists and is close, and that
                // it has been reached — into the transcript the user is reading.
                if let Some(ceiling) = &outcome.ceiling {
                    if ceiling.stopped {
                        let _ = driver_output
                            .send(
                                IOMessage::system_text(format!(
                                    "Spend ceiling reached: {}. No further turn will start. \
                                 Raise --max-cost-usd / --max-tokens and restart to continue.",
                                    ceiling.statement()
                                ))
                                .with_metadata(polaris::plugins::IO_TYPE, "error"),
                            )
                            .await;
                        // The composer must stop looking ready: this session is
                        // taking no more work, whatever the user types.
                        let _ = driver_output
                            .send(polaris_tui::commands::report_unavailable(format!(
                                "spend ceiling reached — {}",
                                ceiling.statement()
                            )))
                            .await;
                    } else if !ceiling_warned
                        && ceiling
                            .used_fraction()
                            .is_some_and(|used| used >= CEILING_WARN_FRACTION)
                    {
                        // Once, at the crossing. Repeating it every turn would
                        // train the user to ignore the line that matters.
                        ceiling_warned = true;
                        let _ = driver_output
                            .send(IOMessage::system_text(format!(
                                "Approaching the spend ceiling: {}.",
                                ceiling.statement()
                            )))
                            .await;
                    }
                }
                // A turn failure is the user's news, not the log buffer's. It
                // lands in the transcript either way; whether the driver keeps
                // serving turns afterwards is `drive_turn_step`'s judgement,
                // bounded by the consecutive-failure ceiling. A ceiling stop
                // carries a sentence too, but the block above has already put it
                // on screen, and it is not a failure — so it is neither repeated
                // nor logged as one.
                let step = budget.absorb(step);
                if outcome.stopped_at_ceiling() {
                    tracing::info!("spend ceiling reached, stopping driver");
                } else if let Some(message) = step.message() {
                    tracing::error!("{message}");
                    let _ = driver_output
                        .send(
                            IOMessage::system_text(message)
                                .with_metadata(polaris::plugins::IO_TYPE, "error"),
                        )
                        .await;
                }
                if !step.continues() {
                    return;
                }
            }
        })
    }
}
