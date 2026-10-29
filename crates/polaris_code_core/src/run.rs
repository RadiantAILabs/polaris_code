//! The run surface: functions that drive a `polaris_code` turn over
//! [`SessionsAPI`], shared unchanged by every front end.
//!
//! [`drive_turn`] runs exactly one turn; [`drive_until`] loops it. The interactive front end spawns
//! [`drive_until`]; the non-interactive front end calls [`drive_turn`] once,
//! after pre-enqueueing its one prompt into whichever
//! [`UserIO`](polaris::plugins::UserIO) plugin the caller installed instead
//! of the TUI panels.
//!
//! # Resilience
//!
//! Three small, separately testable pieces make an interactive session
//! survive the ways a terminal process actually ends. Each is a composition
//! of primitives `polaris-ai` already provides, and none of them owns
//! another's state:
//!
//! - [`persist_turn`] — the bounded `save_session` the driver issues **after
//!   every completed turn**, against the session it just drove. Durability
//!   stops being a property of reaching the exit path.
//! - [`drive_turn_step`] / [`FailureBudget`] — the judgement about which turn
//!   failures the next turn can survive, and the ceiling that stops a driver
//!   from retrying a wedged session forever.
//! - [`stop_signal`] / [`quiesce_turn`] / [`shutdown_driver`] — the orderly
//!   stop: `SIGINT`/`SIGTERM`, a cooperative interrupt of the live turn, and
//!   a final save of **the session in force**, which after a `/new` or
//!   `/resume` is not the one the process launched with.
//!
//! No struct owns the server, session, or IO here. Each front end owns its own
//! assembly, while this module holds the shared outer turn loop. The
//! resilience pieces take the session id, store, and driver handle as
//! arguments rather than holding them together in a host object.
//!
//! # The spend ceiling
//!
//! An operator-set [`RunCeiling`] is consulted here and nowhere else, because
//! this is the only place either front end decides whether to take another
//! turn. The check is on both sides of that boundary and never inside a turn:
//!
//! - **Before** anything starts the turn — above the topology-selection point
//!   in [`drive_turn_with_status`] — a run already at its limit takes no turn
//!   at all and reports [`TurnReason::CeilingReached`].
//! - **After** a turn completes, the same evaluation runs again over the
//!   accounting that turn produced. A completed turn that crossed the line is
//!   reported as [`TurnReason::CeilingReached`] too, so the run that stops
//!   *because of* the ceiling is distinguishable from one that merely
//!   finished. The turn's own work is kept either way: the ceiling never
//!   interrupts a turn in flight.
//!
//! A run with no ceiling installed has none of this behaviour and reports
//! `ceiling: None` — the resource's absence is "no limit was set".

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use polaris::graph::RunId;
use polaris::models::llm::{AssistantBlock, Message};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOProvider};
use polaris::sessions::{SessionError, SessionId, SessionsAPI, TurnNumber};

use crate::agents::{AgentConfig, ContextManager, LoopState};
use crate::cosmos::host::{self, TopologyTurn};
use crate::terminal::{SessionSettings, TurnSteering, transcript};
use crate::usage::{CeilingStatus, RunCeiling, RunUsage, TokenMeter};

type StatusFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Identity source for the answer blocks this module closes.
///
/// [`transcript`]'s own minter is private to the agent's streaming path, which
/// is the right place for it: these ids are stamped from outside any session
/// context. A distinct `t` prefix keeps the two id spaces from ever colliding
/// in one conversation panel.
static NEXT_ANSWER_BLOCK: AtomicU64 = AtomicU64::new(0);

/// Front-end-owned route for the messages the run surface itself publishes:
/// the cancellation lifecycle, and the answer of a turn that ran outside a
/// session context.
///
/// A turn served by the topology-selection point runs before
/// `process_turn_with`, so it can borrow neither the session's
/// [`UserIO`](polaris::plugins::UserIO) nor its
/// [`LiveTranscript`](crate::terminal::LiveTranscript). This is the primitive
/// that already solved that for cancellation — a provider the front end owns,
/// adapted without exposing anything else about the turn — so the answer takes
/// the same route rather than a second transport.
#[derive(Clone)]
pub struct StatusEmitter {
    send: Arc<dyn Fn(IOMessage) -> StatusFuture + Send + Sync>,
}

impl StatusEmitter {
    /// Adapt an existing front-end provider without exposing the cancellation token.
    #[must_use]
    pub fn new<P: IOProvider + 'static>(provider: Arc<P>) -> Self {
        Self {
            send: Arc::new(move |message| {
                let provider = provider.clone();
                Box::pin(async move {
                    let _ = provider.send(message).await;
                })
            }),
        }
    }

    async fn emit(&self, text: &str, io_type: &str) {
        (self.send)(IOMessage::system_text(text).with_metadata(IO_TYPE, io_type)).await;
    }

    /// Publish `text` as one closed assistant block — byte-identical in shape
    /// to what [`LiveTranscript::whole`](crate::terminal::LiveTranscript)
    /// sends, so every front end renders it exactly where it renders a
    /// conversational answer and none of them learns a new message kind.
    async fn answer(&self, text: &str) {
        let id = NEXT_ANSWER_BLOCK.fetch_add(1, Ordering::Relaxed);
        (self.send)(
            IOMessage::from_agent("code", IOContent::Text(text.to_owned()))
                .with_metadata(IO_TYPE, transcript::IO_TYPE_MESSAGE)
                .with_metadata(transcript::META_ID, format!("t{id}"))
                .with_metadata(transcript::META_SEQ, "0")
                .with_metadata(transcript::META_FINAL, transcript::FINAL_TRUE)
                .with_metadata(transcript::META_OUTCOME, transcript::OUTCOME_COMPLETE),
        )
        .await;
    }

    /// Publish a host-level error row into the front end's transcript.
    ///
    /// The same `io_type = "error"` row the interactive front end already
    /// uses for a refused session switch, so a driver-level failure lands
    /// where the user is looking rather than only in the Logs panel.
    pub async fn error(&self, text: &str) {
        self.emit(text, "error").await;
    }
}

/// An orderly request to stop the process, from outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    /// `SIGINT` — a terminal interrupt, or `Ctrl-C` outside the TUI's raw
    /// mode.
    Interrupt,
    /// `SIGTERM` — a supervisor, a container runtime or `kill` asking the
    /// host to stop. Indistinguishable from a kill until someone handles it,
    /// which is the whole reason this exists.
    Terminate,
}

impl StopSignal {
    /// The signal's conventional name, for a log line or a status row.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }

    /// The Unix convention for a signalled exit: `128 + signum`.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Interrupt => 130,
            Self::Terminate => 143,
        }
    }
}

/// Resolve when the process is asked to stop.
///
/// Both front ends watch this instead of `SIGINT` alone: a supervisor's
/// `SIGTERM` is an orderly stop request, and a host that does not handle it
/// is indistinguishable from one that was killed — which is exactly how an
/// interactive session used to lose every turn since launch.
///
/// A platform that refuses one handler still honours the other; a platform
/// that refuses both leaves this future pending forever rather than
/// reporting a stop nobody asked for.
#[cfg(unix)]
pub async fn stop_signal() -> StopSignal {
    use tokio::signal::unix::{SignalKind, signal};

    match (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    ) {
        (Ok(mut interrupt), Ok(mut terminate)) => tokio::select! {
            _ = interrupt.recv() => StopSignal::Interrupt,
            _ = terminate.recv() => StopSignal::Terminate,
        },
        (Ok(mut interrupt), Err(error)) => {
            tracing::warn!("SIGTERM handler unavailable: {error}");
            interrupt.recv().await;
            StopSignal::Interrupt
        }
        (Err(error), Ok(mut terminate)) => {
            tracing::warn!("SIGINT handler unavailable: {error}");
            terminate.recv().await;
            StopSignal::Terminate
        }
        (Err(error), Err(_)) => {
            tracing::error!("no stop-signal handler could be installed: {error}");
            std::future::pending().await
        }
    }
}

/// Resolve when the process is asked to stop.
///
/// Off Unix there is only `Ctrl-C`; an error installing the handler leaves
/// this future pending rather than reporting a stop nobody asked for.
#[cfg(not(unix))]
pub async fn stop_signal() -> StopSignal {
    match tokio::signal::ctrl_c().await {
        Ok(()) => StopSignal::Interrupt,
        Err(error) => {
            tracing::error!("no stop-signal handler could be installed: {error}");
            std::future::pending().await
        }
    }
}

/// Why a turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReason {
    /// The turn ran to completion.
    Completed,
    /// The turn failed; the message is the session error's rendering.
    Failed(String),
    /// The turn was interrupted before completion.
    ///
    /// The upstream session cancellation signal stopped the turn cleanly.
    Interrupted,
    /// The turn accepted a coding task and started no topology for it.
    ///
    /// A typed, explainable terminal answer rather than a failure of the turn
    /// machinery: the decision is persisted as evidence, the interactive
    /// driver continues, and **no default graph runs in the selected one's
    /// place**. [`TurnOutcome::topology`] carries which answer it was —
    /// [`TopologyTurn::NoEligibleCandidate`] or [`TopologyTurn::NotStarted`] —
    /// with the selector's own exclusion codes. Not used for
    /// [`TopologyTurn::Declined`]: a user directive that asked for no topology
    /// is honoured, not failed, and completes like any other served turn.
    NoTopologyStarted,
    /// The run reached the operator's spend ceiling and stops here.
    ///
    /// Not a failure and not an interruption: whatever ran, ran to
    /// completion, the session is saved, and the run ends because the next
    /// turn would spend past what was allowed.
    /// [`TurnOutcome::ceiling`] carries what was spent against what was
    /// allowed, and [`CEILING_EXIT_CODE`] keeps it distinguishable from every
    /// other ending.
    CeilingReached,
    /// A coding task stopped before a model call that could exceed its cap.
    CostCapped,
}

/// The non-interactive exit code for a run that stopped at its spend ceiling.
///
/// Distinct from `0` (the run finished its work), `1` (a turn failed), `2` (a
/// usage or configuration error — including a ceiling that cannot be
/// enforced) and `130` (interrupted), so an unattended caller can tell
/// "stopped because it ran out of budget" from "stopped because it was done"
/// without parsing any output.
pub const CEILING_EXIT_CODE: i32 = 3;

impl TurnReason {
    /// The non-interactive exit code for this reason: `0` completed, `1`
    /// failed, [`CEILING_EXIT_CODE`] stopped at the spend ceiling, `130`
    /// interrupted (the Unix `SIGINT` convention).
    ///
    /// A turn that started no topology exits `1`: the accepted work was not
    /// done, so a `--print` caller must not read it as success.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Failed(_) | Self::NoTopologyStarted => 1,
            Self::CeilingReached => CEILING_EXIT_CODE,
            Self::CostCapped => CEILING_EXIT_CODE,
            Self::Interrupted => 130,
        }
    }
}

/// The result of driving exactly one turn with [`drive_turn`].
#[derive(Debug, Clone)]
pub struct TurnOutcome {
    /// The final assistant text, if the turn produced one — the same text
    /// `finalize` closed the live transcript block with.
    pub final_text: Option<String>,
    /// The executor's run id for this turn, if execution started — the trace
    /// join key into graph events. `None` when the turn never reached the
    /// executor (e.g. the session was busy or missing): the executor mints no
    /// id in that case, so there is nothing honest to report.
    pub run_id: Option<RunId>,
    /// How many graph nodes ran this turn.
    pub nodes_executed: usize,
    /// Wall-clock duration of the turn's graph execution.
    pub duration: Duration,
    /// The session's turn number after this turn: incremented on success,
    /// unchanged on failure — the session's own accounting, read back rather
    /// than derived here.
    pub turn_number: TurnNumber,
    /// Why the turn ended.
    pub reason: TurnReason,
    /// The run's token and cost accounting as of the end of this turn, read
    /// back from the session's own [`TokenMeter`] — the same meter the agent's
    /// `act` system billed while the turn ran, not a second tally.
    ///
    /// Reported for every outcome, including an interrupted one: a turn that
    /// was cut short still spent whatever it spent before stopping, and a
    /// consumer paying for it needs that number.
    ///
    /// `None` means **unmeasured, not zero**: the run installed no
    /// [`TokenMeter`], or the session could not be addressed at all. The
    /// shipped binary always installs one, so a `--print` run reports `Some`.
    pub usage: Option<RunUsage>,
    /// What the topology-selection point decided for this turn.
    ///
    /// `None` is the ordinary conversational turn: the host had accepted no
    /// coding task, so no selection ran and the
    /// [`CodeAgent`](crate::agents::CodeAgent) served the turn as it always
    /// has. It is also what a [`host::TaskOrigin::AutoRouted`] accept reports
    /// once its selection answer turns out
    /// [`not conclusive`](TopologyTurn::is_conclusive): that attempt is still
    /// recorded as Cosmos evidence and logged, but it was not this turn's
    /// answer, so it is not carried here either — the conversational graph
    /// that *did* answer this turn looks exactly like it always has. `Some`
    /// means one accepted coding task reached a conclusive terminal selection
    /// answer, and that answer *was* this turn — the conversational graph did
    /// not also run.
    pub topology: Option<TopologyTurn>,
    /// Where the run stands against its operator-set spend ceiling, evaluated
    /// over the same [`usage`](Self::usage) reported above.
    ///
    /// `None` means **no ceiling was set** — the run installed no
    /// [`RunCeiling`], so there is nothing to stand against. It never means
    /// "within the limit"; that is `Some` with
    /// [`stopped`](CeilingStatus::stopped) false, and it is what a front end
    /// watches to show a run approaching its limit.
    ///
    /// Present on every outcome that has a ceiling, including a failed or
    /// interrupted one: a turn that ended badly still spent what it spent.
    pub ceiling: Option<CeilingStatus>,
}

impl TurnOutcome {
    /// Whether this run stopped because it reached its spend ceiling.
    #[must_use]
    pub fn stopped_at_ceiling(&self) -> bool {
        matches!(self.reason, TurnReason::CeilingReached)
    }
}

/// Drive exactly one turn of `session_id` on `sessions`.
///
/// Seeds a fresh [`LoopState`] and clears prior outputs — exactly what the
/// interactive driver has always done before a turn — then runs one
/// `process_turn_with`.
///
/// The topology-selection point is the marked branch below. It is inert unless
/// the host accepted a coding task for this turn, so a conversational turn is
/// byte-for-byte what it was before selection existed.
///
/// Never panics; a session-level failure is reported as
/// [`TurnReason::Failed`], not propagated.
pub async fn drive_turn(sessions: &SessionsAPI, session_id: &SessionId) -> TurnOutcome {
    drive_turn_with_status(sessions, session_id, None).await
}

/// Drive exactly one turn, publishing cancellation lifecycle messages through
/// the front end's provider when supplied.
///
/// This is the turn boundary, and so it is where the operator's spend ceiling
/// is honoured: [`ceiling_status`] is consulted before the turn starts and
/// again over what the turn billed. See the module docs for why both.
pub async fn drive_turn_with_status(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    status: Option<&StatusEmitter>,
) -> TurnOutcome {
    drive_turn_step(sessions, session_id, status).await.0
}

/// Drive exactly one turn and say what the interactive driver should do next.
///
/// Identical to [`drive_turn_with_status`] but for the second half of the
/// pair: the [`DriverStep`]. A [`TurnReason`] alone cannot answer "can the
/// next turn survive this?", because it carries only the failure's rendered
/// text — the [`SessionError`] variant that knows is consumed here. So the
/// classification is made at the one place that still holds the typed error
/// and handed back alongside the outcome, rather than reconstructed later
/// from a string.
///
/// A run that reached its spend ceiling is the other way a turn ends the
/// driver. It is not a failure, so the step carries what was spent against
/// what was allowed rather than a failure sentence.
pub async fn drive_turn_step(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    status: Option<&StatusEmitter>,
) -> (TurnOutcome, DriverStep) {
    // ── Spend-ceiling point ─────────────────────────────────────────────
    // Above the topology-selection point, so a run already at its limit
    // starts nothing at all: no selection, no graph, no provider call.
    if let Some(ceiling) = ceiling_status(sessions, session_id).await
        && ceiling.stopped
    {
        let turn_number = sessions
            .session_info(session_id)
            .map(|info| info.turn_number)
            .unwrap_or_default();
        let statement = ceiling.statement();
        tracing::info!("spend ceiling reached, taking no further turn: {statement}");
        return (
            TurnOutcome {
                final_text: None,
                run_id: None,
                nodes_executed: 0,
                duration: Duration::ZERO,
                turn_number,
                reason: TurnReason::CeilingReached,
                usage: run_usage(sessions, session_id).await,
                topology: None,
                ceiling: Some(ceiling),
            },
            DriverStep::Stop(statement),
        );
    }

    let (mut outcome, mut step) = run_one_turn(sessions, session_id, status).await;

    // The other side of the same boundary: re-evaluate over what this turn
    // billed. A completed turn that crossed the line ends the run here rather
    // than being cut short above, which is the whole point of checking
    // between turns instead of during one.
    outcome.ceiling = ceiling_status(sessions, session_id).await;
    if outcome.reason == TurnReason::Completed
        && let Some(ceiling) = outcome.ceiling.as_ref().filter(|c| c.stopped)
    {
        outcome.reason = TurnReason::CeilingReached;
        step = DriverStep::Stop(ceiling.statement());
    }
    (outcome, step)
}

/// One turn, with no ceiling in the picture — the body
/// [`drive_turn_step`] wraps.
async fn run_one_turn(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    status: Option<&StatusEmitter>,
) -> (TurnOutcome, DriverStep) {
    // ── Topology-selection point ────────────────────────────────────────
    // Selection happens here, inside the outer session loop and before any
    // graph starts. Nothing above this line has started the turn yet.
    //
    // The turn number read here is the session's *before* this turn; it seeds
    // this turn's evidence event identities and is never renumbered. With no
    // accepted coding task the call is inert and the
    // conversational turn below runs unchanged.
    let pending_turn = sessions
        .session_info(session_id)
        .map(|info| info.turn_number)
        .unwrap_or_default();
    match host::select_and_run(sessions, session_id, pending_turn).await {
        Ok(None) => {}
        Ok(Some(topology))
            if topology.origin() == host::TaskOrigin::AutoRouted && !topology.is_conclusive() =>
        {
            // Nobody confirmed this routing, and the light path had nothing
            // usable to say — `TaskOrigin::AutoRouted`'s one behavioural
            // difference from an explicit accept (see `TopologyTurn::is_conclusive`).
            // The attempt is already durable Cosmos evidence and this line
            // puts it in the trace too, so it is never silent even though it
            // is not this turn's answer: the conversational turn below is.
            tracing::info!(
                "auto-routed task reached no usable conclusion ({}); falling through to the \
                 conversational turn",
                topology.answer()
            );
        }
        Ok(Some(topology)) => {
            let outcome = topology_outcome(sessions, session_id, topology, status).await;
            // A recorded no-start answer is terminal for the turn, not for
            // the driver: `driver_continues` has always said so.
            return (outcome, DriverStep::Continue);
        }
        // A host that accepted a coding task it cannot honour has a wiring
        // fault, not a conversation. Reporting it as a failed turn keeps the
        // accepted task from silently becoming a chat turn — and the fault is
        // in the host's wiring, so the next turn would hit it identically
        // without ever reaching the user for input. That is the shape of
        // failure a driver must not retry.
        Err(error) => {
            let message = format!("topology selection failed: {error}");
            tracing::error!("{message}");
            let outcome = TurnOutcome {
                final_text: None,
                run_id: None,
                nodes_executed: 0,
                duration: Duration::ZERO,
                turn_number: pending_turn,
                reason: TurnReason::Failed(message.clone()),
                usage: run_usage(sessions, session_id).await,
                topology: None,
                ceiling: None,
            };
            return (outcome, DriverStep::Stop(stop_sentence(&message)));
        }
    }

    let result = sessions
        .process_turn_with(session_id, |ctx| {
            if let Ok(steering) = ctx.get_resource::<TurnSteering>()
                && steering.is_enabled()
            {
                steering.activate();
            }
            ctx.insert(LoopState::default());
            ctx.clear_outputs();
        })
        .await;

    let session_switch = sessions
        .with_context(session_id, |ctx| {
            result.is_err()
                && ctx
                    .get_resource::<LoopState>()
                    .is_ok_and(|state| state.is_complete)
        })
        .await
        .unwrap_or(false);

    // Convert any steering that did not reach a safe boundary into future
    // follow-ups on every terminal outcome, including cancellation and error.
    let _ = sessions
        .with_context(session_id, |ctx| {
            if let Ok(steering) = ctx.get_resource::<TurnSteering>()
                && steering.is_enabled()
            {
                steering.finish();
            }
        })
        .await;

    // Read after the call either way: a successful turn's number is already
    // incremented, a failed one's is left exactly where it was — both are
    // "the resulting turn number" for that outcome. `unwrap_or_default`'s `0`
    // is unreachable in practice (we just addressed this exact session), kept
    // only so a theoretical race never turns into a panic here.
    let turn_number = sessions
        .session_info(session_id)
        .map(|info| info.turn_number)
        .unwrap_or_default();

    // Read from the live session context, so what is reported is the meter the
    // turn itself billed and the model id the turn itself ran under, not a
    // copy the front end happened to be holding.
    let usage = run_usage(sessions, session_id).await;

    // A caught model-call failure that `recover` could not also work around
    // (its own recovery call failed too) still leaves the graph reaching its
    // terminal `is_complete` state, so the executor returns `Ok` below. Read
    // `LoopState::failure` back to tell that apart from an ordinary
    // completion — otherwise a failed turn reports `status: "completed"`.
    let recovery_failure = sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<LoopState>()
                .ok()
                .and_then(|state| state.failure.clone())
        })
        .await
        .unwrap_or_default();

    // Read the meter before returning early: a turn that ended by switching
    // sessions still billed whatever it spent, and `usage` is documented as
    // reported for every outcome.
    if session_switch {
        let outcome = TurnOutcome {
            final_text: None,
            run_id: None,
            nodes_executed: 0,
            duration: Duration::ZERO,
            turn_number,
            reason: TurnReason::Completed,
            usage,
            topology: None,
            ceiling: None,
        };
        return (outcome, DriverStep::Continue);
    }

    match result {
        Ok(exec_result) => (
            TurnOutcome {
                final_text: final_assistant_text(sessions, session_id).await,
                run_id: Some(exec_result.run_id().clone()),
                nodes_executed: exec_result.nodes_executed(),
                duration: exec_result.duration(),
                turn_number,
                reason: recovery_failure.map_or(TurnReason::Completed, TurnReason::Failed),
                usage,
                topology: None,
                ceiling: None,
            },
            DriverStep::Continue,
        ),
        Err(SessionError::Interrupted {
            run_id,
            nodes_executed,
            elapsed,
            ..
        }) => {
            // The session context is locked while its graph runs, so external
            // callers cannot safely borrow UserIO at signal time. Publish the
            // two truthful lifecycle events in order once the interrupted run
            // has released that lock.
            if let Some(status) = status {
                status.emit("Interrupting", "interrupting").await;
                status.emit("Cancelled", "cancelled").await;
            }
            (
                TurnOutcome {
                    final_text: None,
                    run_id: Some(run_id),
                    nodes_executed,
                    duration: elapsed,
                    turn_number,
                    reason: TurnReason::Interrupted,
                    usage,
                    topology: None,
                    ceiling: None,
                },
                DriverStep::Continue,
            )
        }
        Err(err) => {
            let message = err.to_string();
            let step = classify_failure(&err, &message);
            (
                TurnOutcome {
                    final_text: None,
                    run_id: None,
                    nodes_executed: 0,
                    duration: Duration::ZERO,
                    turn_number,
                    reason: TurnReason::Failed(message),
                    usage,
                    topology: None,
                    ceiling: None,
                },
                step,
            )
        }
    }
}

/// Where the line falls between a turn failure the driver survives and one
/// that ends the session.
///
/// The judgement is deliberately **not** "keep going unless something looks
/// scary". It is a property of the failure the user can reason about: *did
/// the turn reach the user, and would the next turn be any different?*
///
/// - The turn ran and something inside it broke — a provider hiccup, a tool
///   that returned an error, an iteration ceiling
///   ([`SessionError::Execution`]), or a serialization/store fault raised
///   while recording it ([`SessionError::Persistence`],
///   [`SessionError::Store`]). The user's next message gets a fresh graph
///   with a fresh provider call, and the loop parks on input before trying
///   anything, so retrying is both useful and paced by the user. **Keep the
///   driver alive.**
/// - Everything else is a property of the *session*, not of the turn:
///   [`SessionError::SessionNotFound`], [`SessionError::ReadOnly`],
///   [`SessionError::SessionBusy`], [`SessionError::AgentNotFound`], a
///   failed setup, a graph that does not validate, a restore that left the
///   context in an unknown state. Two things are true of all of them: the
///   next turn fails the same way, and it fails *before* the graph parks on
///   input — so "keep going" is not resilience, it is a hot loop that burns
///   a core and never tells anyone. **Stop the driver, with the reason on
///   screen.**
///
/// [`SessionError`] is `#[non_exhaustive]`, and an unrecognised variant is
/// treated as the second class on purpose: a driver that stops and says why
/// is recoverable by the user; a driver that spins is not.
fn classify_failure(error: &SessionError, message: &str) -> DriverStep {
    match error {
        SessionError::Execution(_) | SessionError::Persistence(_) | SessionError::Store(_) => {
            DriverStep::Recover(format!(
                "Turn failed: {message}. The session is still open — send another message to retry."
            ))
        }
        _ => DriverStep::Stop(stop_sentence(message)),
    }
}

/// The sentence a front end shows when the driver is about to stop.
///
/// It does not claim the session was saved. The save is a separate step with
/// its own failure sentence, and a stop message that asserts durability it
/// did not witness would be misleading.
fn stop_sentence(message: &str) -> String {
    format!(
        "Turn failed: {message}. This session cannot serve another turn; \
         restart `polaris-code` to continue it."
    )
}

/// Reports a turn the topology-selection point served.
///
/// The session's own accounting is read back rather than derived: a selected
/// topology runs in its own context outside `process_turn_with`, so the
/// session's turn number does not advance. That run context carries a
/// *separate* meter a graph node's model call bills first, broken out on
/// `ExecutedTopology::usage`; by the time this reads the session's own
/// [`TokenMeter`], that run has already folded the same spend onto it, so it
/// is included here too rather than only on the per-run figure.
///
/// [`TopologyTurn::answer`] is the turn's answer for **every** variant, and it
/// is published twice on purpose, because a front end reads exactly one of the
/// two: [`TurnOutcome::final_text`] is what `exec::report` renders for a
/// `--print` caller, and the [`StatusEmitter`] block is what an interactive
/// panel renders. Neither is a second rendering — both carry the same string,
/// produced once here.
async fn topology_outcome(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    topology: TopologyTurn,
    status: Option<&StatusEmitter>,
) -> TurnOutcome {
    let turn_number = sessions
        .session_info(session_id)
        .map(|info| info.turn_number)
        .unwrap_or_default();
    let usage = run_usage(sessions, session_id).await;
    let answer = topology.answer();
    if let Some(status) = status {
        status.answer(&answer).await;
    }
    match &topology {
        TopologyTurn::Executed(run) => TurnOutcome {
            final_text: Some(answer),
            run_id: Some(run.run_id.clone()),
            nodes_executed: run.nodes_executed,
            duration: run.duration,
            turn_number,
            reason: TurnReason::Completed,
            usage,
            topology: Some(topology),
            ceiling: None,
        },
        TopologyTurn::CostCapped(_) => TurnOutcome {
            final_text: Some(answer),
            run_id: None,
            nodes_executed: 0,
            duration: Duration::ZERO,
            turn_number,
            reason: TurnReason::CostCapped,
            usage,
            topology: Some(topology),
            ceiling: None,
        },
        // A terminal no-start answer *is* the turn's answer, so it is reported
        // in the same field a served turn's is. `reason` and the exit code —
        // the fields that exist to say so — are what still distinguish it.
        TopologyTurn::NoEligibleCandidate(_) | TopologyTurn::NotStarted(_) => TurnOutcome {
            final_text: Some(answer),
            run_id: None,
            nodes_executed: 0,
            duration: Duration::ZERO,
            turn_number,
            reason: TurnReason::NoTopologyStarted,
            usage,
            topology: Some(topology),
            ceiling: None,
        },
        // Unlike the other two no-start answers, a decline is not a failed
        // request — it is exactly what the user asked for, fully honoured, so
        // it completes like an ordinary served turn rather than exiting `1`.
        // "An honoured request must actually run what was asked for" applies
        // to "run nothing" too.
        TopologyTurn::Declined(_) => TurnOutcome {
            final_text: Some(answer),
            run_id: None,
            nodes_executed: 0,
            duration: Duration::ZERO,
            turn_number,
            reason: TurnReason::Completed,
            usage,
            topology: Some(topology),
            ceiling: None,
        },
    }
}

/// What the interactive driver must do after one turn.
///
/// Carries the sentence to put on screen as well as the decision, because
/// the two were the same bug: a failure that only reached `tracing::error!`
/// left the user looking at an idle-seeming app with the reason on another
/// panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverStep {
    /// Start another turn; there is nothing to tell the user.
    Continue,
    /// The turn failed for a reason the next turn can survive. Show the
    /// sentence, then start another turn.
    Recover(String),
    /// The turn failed in a way the next turn cannot survive. Show the
    /// sentence, then stop the driver.
    Stop(String),
}

impl DriverStep {
    /// Whether the driver should begin another turn.
    #[must_use]
    pub fn continues(&self) -> bool {
        !matches!(self, Self::Stop(_))
    }

    /// The sentence the front end should show, if this step has one.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Continue => None,
            Self::Recover(message) | Self::Stop(message) => Some(message),
        }
    }
}

/// How many consecutive recoverable failures an interactive driver absorbs
/// before it stops anyway.
///
/// Recoverable is a judgement about one failure, not a promise about the
/// next one. A session that fails three turns in a row is not hiccuping, and
/// a driver that never gives up is a worse bug than one that gives up too
/// early: the user gets the same unexplained turn back forever.
#[derive(Debug, Clone)]
pub struct FailureBudget {
    consecutive: u32,
    limit: u32,
}

impl FailureBudget {
    /// The shipped ceiling: three failures in a row end the driver.
    pub const DEFAULT_LIMIT: u32 = 3;

    /// A budget at [`Self::DEFAULT_LIMIT`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(Self::DEFAULT_LIMIT)
    }

    /// A budget with an explicit ceiling. `0` disables the ceiling.
    #[must_use]
    pub fn with_limit(limit: u32) -> Self {
        Self {
            consecutive: 0,
            limit,
        }
    }

    /// Failed turns since the last turn that did not fail.
    #[must_use]
    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive
    }

    /// Fold one turn's step into the budget and return the step the loop must
    /// actually act on.
    ///
    /// [`DriverStep::Recover`] becomes [`DriverStep::Stop`] once the ceiling
    /// is reached — same sentence, plus why the driver is giving up on it.
    pub fn absorb(&mut self, step: DriverStep) -> DriverStep {
        match step {
            DriverStep::Continue => {
                self.consecutive = 0;
                DriverStep::Continue
            }
            DriverStep::Stop(message) => {
                self.consecutive = self.consecutive.saturating_add(1);
                DriverStep::Stop(message)
            }
            DriverStep::Recover(message) => {
                self.consecutive = self.consecutive.saturating_add(1);
                if self.limit > 0 && self.consecutive >= self.limit {
                    DriverStep::Stop(format!(
                        "{message} Giving up after {} consecutive failed turns.",
                        self.consecutive
                    ))
                } else {
                    DriverStep::Recover(message)
                }
            }
        }
    }
}

impl Default for FailureBudget {
    fn default() -> Self {
        Self::new()
    }
}

/// How long a save is allowed to take before the caller stops waiting.
///
/// The driver's per-turn save and the host's shutdown save share it: a
/// wedged store must not hang the next turn any more than it may hang exit.
pub const SAVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Persist `session_id` to the host's configured store, under a bound.
///
/// This is the durability point. `save_session` already writes atomically
/// through whatever store the host configured and already carries the
/// retained checkpoint set with it, so no second store and no new format is
/// involved — only *when* the call happens and *which* session it names.
///
/// Call it after every turn: a checkpoint capture is an in-memory boundary,
/// so until something saves, a `SIGTERM`, an OOM or a closed terminal takes
/// every turn since launch with it.
///
/// **Call it between turns.** `process_turn_with` holds the session's
/// context lock for the whole graph — including while the graph is parked in
/// `receive_user_input` — so a save issued while a turn is live waits for
/// that turn rather than racing it, and will usually spend the whole
/// [`SAVE_TIMEOUT`] doing so.
///
/// # Errors
///
/// Returns the sentence a front end should show: the store's own error, or
/// the fact that the save exceeded [`SAVE_TIMEOUT`].
pub async fn persist_turn(sessions: &SessionsAPI, session_id: &SessionId) -> Result<(), String> {
    match tokio::time::timeout(SAVE_TIMEOUT, sessions.save_session(session_id)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "Failed to save session `{}`: {error}.",
            session_id.as_str()
        )),
        Err(_) => Err(format!(
            "Saving session `{}` timed out after {}s; the store is not responding.",
            session_id.as_str(),
            SAVE_TIMEOUT.as_secs()
        )),
    }
}

/// Ask a live turn to stop and wait, bounded, for it to let the session go.
///
/// Two existing primitives and nothing else: the session's own cancellation
/// signal, then a context acquisition that can only succeed once the
/// interrupted turn has released the context lock. That second half is the
/// point — an interrupted turn is *recorded* before it unwinds, so waiting
/// for it means the save that follows writes a consistent history instead of
/// whatever a dropped mid-graph future left behind.
///
/// Returns `true` if a turn was actually in flight.
pub async fn quiesce_turn(sessions: &SessionsAPI, session_id: &SessionId, grace: Duration) -> bool {
    if !request_interrupt(sessions, session_id) {
        return false;
    }
    if tokio::time::timeout(grace, sessions.with_context(session_id, |_| ()))
        .await
        .is_err()
    {
        tracing::warn!(
            "interrupted turn of session `{}` did not release within {}s",
            session_id.as_str(),
            grace.as_secs_f32()
        );
    }
    true
}

/// Stop an interactive turn-driver without losing its work.
///
/// `active` must be the session **in force now** — the front end's own
/// current id. A host that has honoured a `/new` or `/resume` has moved it,
/// and saving the id the process launched with writes the wrong
/// conversation, or none at all.
///
/// The order is the whole content of this function, and it is not
/// rearrangeable:
///
/// 1. [`quiesce_turn`] — a live turn is asked to stop and given a bounded
///    moment to record itself.
/// 2. `driver.abort()` — the loop is stopped so it cannot start a turn that
///    races the save. Until this returns, the driver may be parked inside
///    `receive_user_input`, which holds the session's context lock.
/// 3. [`persist_turn`] — the final save, of `active`.
///
/// Nothing here is owned: the sessions handle, the session id and the driver
/// task all belong to the front end, and are borrowed for the length of the
/// call.
///
/// # Errors
///
/// Returns the sentence a front end should show if the final save failed.
pub async fn shutdown_driver(
    sessions: &SessionsAPI,
    active: &SessionId,
    driver: &tokio::task::JoinHandle<()>,
    grace: Duration,
) -> Result<(), String> {
    quiesce_turn(sessions, active, grace).await;
    driver.abort();
    persist_turn(sessions, active).await
}

/// Drive `session_id` until a failure the next turn cannot survive.
///
/// Mirrors the interactive driver's cadence exactly: each call is parked
/// inside `drive_turn`'s `receive_user_input` until a user submits, so the
/// loop is paced by the user, not by this function.
///
/// Every completed turn is persisted before the next one starts
/// ([`persist_turn`]), so the conversation survives a stop this loop never
/// sees. Interrupted turns and recorded no-start answers are resumable;
/// a failed turn is classified by [`drive_turn_step`] and absorbed by a
/// [`FailureBudget`], and whatever the outcome the user is told through
/// `status` rather than only through `tracing`.
///
/// A run that reaches its spend ceiling stops the loop for the same reason a
/// failed turn does — there is no next turn to take — but cleanly: the turn
/// that just ran is complete, the sentence says what was spent against what
/// was allowed, and it is logged as information rather than as an error.
pub async fn drive_until(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    status: Option<&StatusEmitter>,
) {
    let mut budget = FailureBudget::new();
    loop {
        let (outcome, step) = drive_turn_step(sessions, session_id, status).await;
        if let Err(message) = persist_turn(sessions, session_id).await {
            tracing::error!("{message}");
            if let Some(status) = status {
                status.error(&message).await;
            }
        }
        let step = budget.absorb(step);
        if let Some(message) = step.message() {
            if outcome.stopped_at_ceiling() {
                tracing::info!("spend ceiling reached, stopping driver: {message}");
            } else {
                tracing::error!("{message}");
            }
            if let Some(status) = status {
                status.error(message).await;
            }
        }
        if !step.continues() {
            return;
        }
    }
}

/// Raise the live session's cancellation signal and publish the host-truthful
/// cancellation signal when a turn is actually in flight.
///
/// Returns `false` for an idle or unknown session. Status publication happens
/// in [`drive_turn`] after the running graph releases its context lock.
pub fn request_interrupt(sessions: &SessionsAPI, session_id: &SessionId) -> bool {
    let Ok(was_running) = sessions.interrupt_turn(session_id) else {
        return false;
    };
    was_running
}

/// Whether a turn ended in a way that is resumable on its face.
///
/// The reason-only view, and deliberately the conservative one: a
/// [`TurnReason`] carries the failure's rendered text and nothing else, so
/// from a reason alone every failure has to be assumed fatal. A turn that
/// started no topology is resumable — the decision was recorded, nothing
/// ran, and the next user submission gets a fresh turn. A run that reached
/// its spend ceiling stops the driver too, the difference being that the
/// ceiling is not an error, so the front end reports what was spent against
/// what was allowed rather than a failure.
///
/// An interactive driver should use [`drive_turn_step`] instead, which
/// classifies the failure while it still holds the typed
/// [`SessionError`](polaris::sessions::SessionError); this stays for callers
/// that hold only an outcome.
#[must_use]
pub fn driver_continues(reason: &TurnReason) -> bool {
    !matches!(reason, TurnReason::Failed(_) | TurnReason::CeilingReached)
}

/// Where `session_id` stands against the operator's spend ceiling right now.
///
/// Reads the [`RunCeiling`] and the accounting out of the same session
/// context the turn ran in, so the limit being enforced is the one the
/// operator set for this run and the spend being judged is the one the run
/// reports. Returns `None` when no ceiling was installed — the absence of the
/// resource is "no limit was set", never a limit of zero.
pub async fn ceiling_status(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> Option<CeilingStatus> {
    let ceiling = sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<RunCeiling>().ok().map(|res| *res)
        })
        .await
        .ok()
        .flatten()?;
    Some(ceiling.evaluate(&run_usage(sessions, session_id).await?))
}

/// Freeze `session_id`'s accounting into the [`RunUsage`] a front end reports.
///
/// Reads the [`TokenMeter`] and the model out of the session context — the
/// same context the agent's systems ran in, so the meter is the one `act`
/// incremented and the model id is the one the turn actually drove. Nothing
/// here recomputes a count or re-estimates a cost; the snapshot is a read of
/// what the run already accumulated.
///
/// The model is [`SessionSettings::served_model`] where a turn has run — the
/// id the agent resolved, after any `/model` swap and after the registry has
/// either honoured or refused it — falling back to [`AgentConfig`] for a
/// session that never reached the agent's preparation step. A record naming a
/// model the run did not use would be worse than one naming none, so the
/// requested id is never reported as the served one.
///
/// Returns `None` when the session has no meter (the binary always installs
/// one) or cannot be addressed, so an unmeasured run is reported as unmeasured
/// rather than as a run that cost nothing.
async fn run_usage(sessions: &SessionsAPI, session_id: &SessionId) -> Option<RunUsage> {
    sessions
        .with_context(session_id, |ctx| {
            let meter = ctx.get_resource::<TokenMeter>().ok()?;
            let model_id = ctx
                .get_resource::<SessionSettings>()
                .ok()
                .and_then(|settings| settings.served_model())
                .or_else(|| Some(ctx.get_resource::<AgentConfig>().ok()?.model_id.clone()))?;
            Some(meter.snapshot(&model_id))
        })
        .await
        .ok()
        .flatten()
}

/// The last assistant message's text in `session_id`'s history, if any.
///
/// Reads the same [`ContextManager`] both front ends already share, rather
/// than tapping any particular turn's `UserIO` stream, so this has no
/// dependency on which `IOProvider` was behind the turn.
async fn final_assistant_text(sessions: &SessionsAPI, session_id: &SessionId) -> Option<String> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .ok()
                .and_then(|history| history.messages.last().and_then(assistant_text))
        })
        .await
        .ok()
        .flatten()
}

/// The concatenated text blocks of an assistant message, or `None` for a user
/// message or a purely non-text assistant message (e.g. tool calls only).
fn assistant_text(message: &Message) -> Option<String> {
    let Message::Assistant { content, .. } = message else {
        return None;
    };
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            AssistantBlock::Text(block) => Some(block.text.as_str()),
            _ => None,
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_the_cli_contract() {
        assert_eq!(TurnReason::Completed.exit_code(), 0);
        assert_eq!(TurnReason::Failed("boom".into()).exit_code(), 1);
        assert_eq!(TurnReason::Interrupted.exit_code(), 130);
        assert_eq!(
            TurnReason::NoTopologyStarted.exit_code(),
            1,
            "accepted work that never ran is not a successful run"
        );
        assert_eq!(TurnReason::CeilingReached.exit_code(), CEILING_EXIT_CODE);
    }

    #[test]
    fn the_ceiling_exit_code_is_distinguishable_from_every_other_ending() {
        let codes = [
            TurnReason::Completed.exit_code(),
            TurnReason::Failed("boom".into()).exit_code(),
            TurnReason::Interrupted.exit_code(),
            TurnReason::CeilingReached.exit_code(),
        ];
        assert_eq!(codes, [0, 1, 130, 3]);
        assert!(
            !codes[..3].contains(&CEILING_EXIT_CODE),
            "stopping at the budget must not be confused with any other ending"
        );
        // `2` is reserved for usage and configuration errors, which is what an
        // unenforceable ceiling is reported as before the run starts.
        assert_ne!(CEILING_EXIT_CODE, 2);
    }

    #[test]
    fn interactive_driver_continues_only_resumable_outcomes() {
        assert!(driver_continues(&TurnReason::Completed));
        assert!(driver_continues(&TurnReason::Interrupted));
        assert!(
            driver_continues(&TurnReason::NoTopologyStarted),
            "a recorded no-start answer is a terminal turn, not a broken driver"
        );
        assert!(!driver_continues(&TurnReason::Failed("boom".into())));
        assert!(
            !driver_continues(&TurnReason::CeilingReached),
            "the next turn is exactly what the ceiling forbids"
        );
    }

    #[test]
    fn a_turn_that_ran_is_recoverable_and_a_broken_session_is_not() {
        let recoverable = classify_failure(
            &SessionError::Store("disk hiccup".into()),
            "store error: disk hiccup",
        );
        assert!(
            matches!(recoverable, DriverStep::Recover(_)),
            "a fault raised while recording a turn does not break the session: {recoverable:?}"
        );

        for error in [
            SessionError::SessionNotFound(SessionId::from_string("gone")),
            SessionError::ReadOnly(SessionId::from_string("frozen")),
            SessionError::SessionBusy(SessionId::from_string("busy")),
            SessionError::AgentNotFound("code".into()),
        ] {
            let message = error.to_string();
            let step = classify_failure(&error, &message);
            assert!(
                matches!(step, DriverStep::Stop(_)),
                "`{message}` fails again before the next turn ever parks on input, \
                 so continuing is a hot loop: {step:?}"
            );
        }
    }

    #[test]
    fn every_step_that_is_not_continue_carries_a_sentence_for_the_user() {
        assert_eq!(DriverStep::Continue.message(), None);
        assert_eq!(
            DriverStep::Recover("retry".into()).message(),
            Some("retry"),
            "a recoverable failure must be shown to the user"
        );
        assert_eq!(DriverStep::Stop("done".into()).message(), Some("done"));
        assert!(DriverStep::Continue.continues());
        assert!(DriverStep::Recover("retry".into()).continues());
        assert!(!DriverStep::Stop("done".into()).continues());
    }

    #[test]
    fn the_failure_budget_gives_up_rather_than_retrying_forever() {
        let mut budget = FailureBudget::new();
        for attempt in 1..FailureBudget::DEFAULT_LIMIT {
            let step = budget.absorb(DriverStep::Recover("provider hiccup".into()));
            assert!(
                step.continues(),
                "failure {attempt} of {} must still return the user to input",
                FailureBudget::DEFAULT_LIMIT
            );
        }
        let step = budget.absorb(DriverStep::Recover("provider hiccup".into()));
        assert!(
            !step.continues(),
            "a session that fails every turn is not hiccuping: {step:?}"
        );
        assert!(
            step.message()
                .is_some_and(|text| text.contains("Giving up")),
            "giving up must say so on screen: {step:?}"
        );
    }

    #[test]
    fn one_good_turn_clears_the_budget() {
        let mut budget = FailureBudget::new();
        budget.absorb(DriverStep::Recover("hiccup".into()));
        budget.absorb(DriverStep::Recover("hiccup".into()));
        assert_eq!(budget.consecutive_failures(), 2);
        budget.absorb(DriverStep::Continue);
        assert_eq!(
            budget.consecutive_failures(),
            0,
            "the ceiling counts failures in a row, not failures in a session"
        );
        assert!(
            budget
                .absorb(DriverStep::Recover("hiccup".into()))
                .continues(),
            "an isolated failure after a good turn must not end the session"
        );
    }

    #[test]
    fn stop_signals_report_the_unix_convention() {
        assert_eq!(StopSignal::Interrupt.exit_code(), 130);
        assert_eq!(StopSignal::Terminate.exit_code(), 143);
        assert_eq!(StopSignal::Interrupt.name(), "SIGINT");
        assert_eq!(StopSignal::Terminate.name(), "SIGTERM");
    }

    #[test]
    fn assistant_text_ignores_user_messages_and_empty_content() {
        assert_eq!(assistant_text(&Message::user("hi")), None);
        assert_eq!(
            assistant_text(&Message::assistant("hello")),
            Some("hello".to_string())
        );
        assert_eq!(
            assistant_text(&Message::Assistant {
                id: None,
                content: vec![],
            }),
            None
        );
    }
}
