//! Topology selection at the live host loop's selection point.
//!
//! [`crate::run::drive_turn`] calls [`select_and_run`] inside the outer session
//! loop, *before* `process_turn_with`. Everything below implements the ordered
//! lifecycle diagrammed here, and changes none of it:
//!
//! ```text
//! outer session / user-input loop                    polaris_code owns
//!   -> take one accepted CodingTask                  polaris_code owns
//!   -> project TaskProfile + explicit Objective      polaris_code + Cosmos values
//!   -> read the sealed TopologyCatalog snapshot      Cosmos catalog capability
//!   -> observe host availability                     polaris_code owns
//!   -> mechanical preflight                          Cosmos owns semantics
//!   -> select and persist SelectionRecord            selector point + host append
//!   -> if selected, construct pristine context       polaris_code owns
//!   -> generation-checked live readmission           Cosmos + Polaris validation
//!   -> append admission and RunOpened                polaris_code host lifecycle
//!   -> GraphExecutor::execute selected native Graph  Polaris executes; host calls
//!   -> append RunFinalized                           host records mechanics only
//!   -> append zero or more OutcomeObservation values polaris_code verifiers
//!   -> return control to outer session loop          polaris_code owns
//! ```
//!
//! Three properties are load-bearing rather than incidental:
//!
//! - **The branch is inert without an accepted coding task.** A host that
//!   installs no [`CodingTaskQueue`], or whose queue is empty, gets `None` and
//!   the conversational [`CodeAgent`](crate::agents::CodeAgent) turn runs
//!   byte-for-byte as it always has. Selection never inspects user text and
//!   never guesses that a turn is "a coding task".
//! - **[`SelectionOutcome::NoEligibleCandidate`] is a terminal answer.** It is
//!   persisted as a [`SelectionRecord`] and returned as
//!   [`TopologyTurn::NoEligibleCandidate`], carrying the selector's own
//!   diagnostics. It never falls through to a default graph — not to another
//!   candidate, and not to the conversational agent.
//! - **At most one registration starts per decision.** Pre-start fallback walks
//!   only the fallback plan the decision captured, against the same catalog
//!   generation and digest that decision ranked. Once the graph has started
//!   there is no retry and no second topology.
//!
//! That last property is enforced by the type system rather than by a runtime
//! check, and it holds no matter what accepts the work: a `StartPermit` is
//! consumed *by value*, so one admission can be started exactly once.
//!
//! ```
//! use polaris_cosmos::contracts::{RegistrationSelectionRef, SelectionDecisionId};
//! use polaris_cosmos::topology::{StartPermit, StartPermitError};
//! use polaris::system::param::SystemContext;
//!
//! // One start of one admission: this is the shape `attempt_live_admission`
//! // uses, and it compiles.
//! fn start_once<'context>(
//!     permit: StartPermit<'context>,
//!     decision: SelectionDecisionId,
//!     candidate: &RegistrationSelectionRef,
//!     context: &SystemContext<'context>,
//! ) -> Result<RegistrationSelectionRef, StartPermitError> {
//!     permit.consume(decision, candidate, context)
//! }
//! ```
//!
//! ```compile_fail,E0382
//! use polaris_cosmos::contracts::{RegistrationSelectionRef, SelectionDecisionId};
//! use polaris_cosmos::topology::StartPermit;
//! use polaris::system::param::SystemContext;
//!
//! // A second start of the same admission is a borrow-check error, not a
//! // refusal at runtime — there is no path an acceptance affordance could add
//! // that reaches it.
//! fn start_twice<'context>(
//!     permit: StartPermit<'context>,
//!     decision: SelectionDecisionId,
//!     candidate: &RegistrationSelectionRef,
//!     context: &SystemContext<'context>,
//! ) {
//!     let _first = permit.consume(decision, candidate, context);
//!     let _second = permit.consume(decision, candidate, context);
//! }
//! ```
//!
//! [`crate::cosmos::proof`] drives the same contracts offline with injected
//! event identities; this module is the same sequence wired to a live session.

use std::any::TypeId;
use std::collections::BTreeSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use polaris::graph::{GraphExecutor, GraphSignature, MiddlewareAPI, RunCancellation};
use polaris::models::ModelRegistry;
use polaris::plugins::UserIO;
use polaris::sessions::{SessionId, SessionsAPI, TurnNumber};
use polaris::system::param::SystemContext;
use polaris::system::plugin;
use polaris::system::plugin::{Contract, Plugin, Version};
use polaris::system::resource::{GlobalResource, LocalResource};
use polaris_cosmos::cognition::approved::coverage_sweep;
use polaris_cosmos::contracts::{
    AdmissionAttemptId, AdmissionEvaluation, AdmissionResult, AssessmentConclusion,
    AvailabilitySnapshotId, CandidateConsideration, CompositionDecision,
    CompositionRevisionEvidence, CompositionRevisionTrail, CompositionValidation, ConclusionKind,
    ConfigurationSchemaKey, ContentDigest, CoveragePpm, DurationMs, EmbodimentId, EmbodimentKey,
    EpistemicBasis, ErrorClassId, ExclusionCode, ExclusionPhase, ExclusionReason,
    ExecutionDisposition, ExecutionFault, ExecutionProvenance, ExecutionStage, HostRuntimeId,
    HostRuntimeKey, LogicalSequence, ModelId, NativeGraphSignatureWitness, Objective,
    OutcomeObservation, OutcomeObservationId, RecordSubjectId, RecordedAt,
    RegistrationSelectionRef, RevisionAttempt, RunCompletion, RunControlFlow, RunFaults,
    RunFinalized, RunId, RunOpened, RunStructure, RunUsage, RuntimeAvailabilitySnapshot,
    SelectionDecision, SelectionDecisionId, SelectionNotStarted, SelectionOutcome,
    SelectionRationale, SelectionRecord, SourceKind, StopReason, TaskId, TokenAccountingId,
    TokenQuantity, TokenUsage,
};
use polaris_cosmos::evidence::{EvidenceEvent, TopologyEvidence};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    AdmissionCandidate, MaterializedGraphAdmissionError, TopologyCatalog, TopologyCatalogSnapshot,
    TopologyContract, preflight_catalog, readmit_live, readmit_materialized_graph,
};

use crate::progress::GraphInstrumentation;

use super::domains;
use super::gate_blocks;
use super::payload::{AppliedEdit, CodingQuestion, CodingWorkspace, EditSite, SweepWorklist};
use super::plan_history;
use super::planner;
use super::task::{CodingIntent, CodingObjective, CodingTask, SuccessCheck};
use super::topology::{self, HostCapabilities};
use super::{composition, graphs, policy, provenance, verify};
use crate::agents::AgentConfig;
use crate::usage::{TaskCostLimit, TokenMeter};

/// What the user said about *which* topology should run this task, if
/// anything.
///
/// This is the whole of the per-turn control surface: naming a topology or
/// turning selection off are both **commands**, not preferences the selector
/// merely weighs — [`select_and_run`] either runs exactly what was asked for
/// or refuses and says why, and it never falls back to its own pick in either
/// case. Scoped to the accepted task itself (not the turn, and not a sticky
/// session setting): the [`CodingTaskQueue`] is already the one channel a
/// piece of coding work travels through, so the directive rides with it
/// rather than adding a second, forgettable place to look.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TopologyDirective {
    /// No request: the selector picks among every admitted candidate as it
    /// always has.
    #[default]
    Automatic,
    /// Run exactly this registration, named by its stable catalog id, or
    /// refuse and say on what ground.
    ///
    /// Composed from the same hard-constraint channel
    /// [`CodingIntent::Answer`](super::task::CodingIntent::Answer) already
    /// uses to forbid every source-mutating shape: every candidate always
    /// carries its own capability tag (`policy::forbid_all_but`), and this
    /// objective forbids every tag but the named one. A candidate mechanical
    /// admission already refused stays refused — narrowing can only shrink the
    /// admitted set the selector ranks over, never admit past it.
    Only(String),
    /// Run no topology for this task.
    ///
    /// Distinct from an empty queue: the task *was* accepted, and this says so
    /// plainly rather than leaving the turn silent. Nothing is selected,
    /// nothing is appended to the evidence store, and the workspace is
    /// untouched — the honest reading of "off" is "runs nothing", never "runs
    /// the default topology quietly".
    None,
}

/// An operator's run-wide constraint on which topology may run, set once from
/// outside the built binary before any turn runs and never touched again.
///
/// `main.rs` reads `POLARIS_CODE_TOPOLOGY_PIN` the same way it already reads
/// `POLARIS_CODE_GRAPH_SEARCH` — an environment gate resolved at startup,
/// validated against the sealed catalog, and installed as a
/// [`GlobalResource`] before any session context exists. Trying a different
/// registration against the same accepted work is then a different process
/// launch, never a rebuild.
///
/// **Scope and precedence — narrower than [`TopologyDirective`], not a second
/// copy of it.** A pin is a *default* for the run, not a command that
/// overrides one: [`select_and_run`] applies it only to a task whose own
/// [`TopologyDirective`] is [`Automatic`](TopologyDirective::Automatic) —
/// exactly the same "the nearer setting wins" precedent `--model` already
/// establishes over `POLARIS_CODE_MODEL` (`main.rs`). A task that names its
/// own `topology=` field, or asks for [`None`](TopologyDirective::None), is
/// answered exactly as it would be with no pin configured at all; the pin
/// never overrides an explicit per-task word, it only fills the silence when
/// there is none.
///
/// **A command, not a preference — same as `Only`.** Once a pin does apply,
/// it is composed through the identical [`narrow_to`] hard-constraint channel
/// [`TopologyDirective::Only`] uses: run exactly this registration or refuse
/// and say why, never fall back to an unconstrained pick and never admit past
/// what mechanical admission already refused.
///
/// **Recorded as what it is.** [`ExecutedTopology::requested_by_pin`] and
/// [`NoTopologyStarted::requested_by_pin`] are `true` exactly when the pin,
/// not the task's own directive, is why `requested` is `Some` — so a later
/// reader of the answer can tell "the operator required this" from "the
/// caller asked for this" from "the selector's own evidence-informed pick",
/// which are three different facts this run-wide constraint must not blur
/// into one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorTopologyPin {
    registration_id: String,
}

impl LocalResource for OperatorTopologyPin {}
impl GlobalResource for OperatorTopologyPin {}

impl Contract for OperatorTopologyPin {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl OperatorTopologyPin {
    /// Pins the run to `registration_id`.
    ///
    /// Not validated here — the caller (`main.rs`) validates against the
    /// sealed catalog snapshot at startup, the same "unknown" ground
    /// `--topology-task`'s own `topology=` field is validated against, so a
    /// typo is a startup refusal naming what is registered, never a value
    /// this type silently accepts and [`select_and_run`] later fails on.
    #[must_use]
    pub fn new(registration_id: impl Into<String>) -> Self {
        Self {
            registration_id: registration_id.into(),
        }
    }

    /// Returns the pinned registration id.
    #[must_use]
    pub fn registration_id(&self) -> &str {
        &self.registration_id
    }
}

/// Where an accepted coding task came from, and what a terminal selection
/// answer that reaches no usable conclusion is allowed to do about it.
///
/// This is the one property `select_and_run`'s outcome handling is allowed to
/// branch on by task origin, and it is deliberately narrow. An
/// [`Explicit`](Self::Explicit) accept — `/topology`, `--topology-task` — is
/// the behaviour this module has always had: [`SelectionOutcome::NoEligibleCandidate`]
/// and a refused live readmission are terminal for the turn, full stop, never
/// falling through to a default graph. That rule stands, unrevised, for every
/// explicit accept.
///
/// An [`AutoRouted`](Self::AutoRouted) accept is derived from ordinary
/// conversational text by [`auto_route`], with nobody asked to confirm it. For
/// that origin alone, a selection answer that is not a usable conclusion —
/// [`TopologyTurn::NoEligibleCandidate`], [`TopologyTurn::NotStarted`], or an
/// [`Executed`](TopologyTurn::Executed) run whose product
/// [`ConclusionKind::Escalated`] — is not the turn's answer: the caller falls
/// through to the ordinary conversational turn instead, within the same
/// request. See [`TopologyTurn::is_conclusive`] for where that check lives and
/// `docs/topology-selection.md` for the rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskOrigin {
    /// The host's caller named this task explicitly.
    #[default]
    Explicit,
    /// [`auto_route`] derived this task from conversational text that nobody
    /// asked to confirm.
    AutoRouted,
}

/// One coding task the host has accepted, with the explicit objective it is
/// selected under and the capabilities its run context will actually have.
///
/// The host decides what "accepted" means — a command, an API call, a scripted
/// driver. The run surface only consumes what was accepted; selection itself
/// still never derives a coding task from conversational text — [`auto_route`]
/// does that once, outside selection, and hands back the same accepted-task
/// type an explicit accept produces.
#[derive(Debug, Clone)]
pub struct AcceptedCodingTask {
    task: CodingTask,
    objective: CodingObjective,
    host: HostCapabilities,
    directive: TopologyDirective,
    origin: TaskOrigin,
}

impl AcceptedCodingTask {
    /// Accepts `task` under the objective its own intent implies, in a
    /// hermetic run context, as an explicit accept.
    ///
    /// [`HostCapabilities::hermetic`] is the honest default: a host that has
    /// wired a test runner or a model provider into the *run context* says so
    /// with [`with_host_capabilities`](Self::with_host_capabilities) rather
    /// than having availability guessed from the environment.
    #[must_use]
    pub const fn new(task: CodingTask) -> Self {
        let objective = CodingObjective::new(task.intent());
        Self {
            task,
            objective,
            host: HostCapabilities::hermetic(),
            directive: TopologyDirective::Automatic,
            origin: TaskOrigin::Explicit,
        }
    }

    /// Overrides the objective the task is ranked under.
    #[must_use]
    pub const fn with_objective(mut self, objective: CodingObjective) -> Self {
        self.objective = objective;
        self
    }

    /// Declares what the run context this task executes in will supply.
    #[must_use]
    pub const fn with_host_capabilities(mut self, host: HostCapabilities) -> Self {
        self.host = host;
        self
    }

    /// States which topology this task's turn must run, if any.
    #[must_use]
    pub fn with_topology_directive(mut self, directive: TopologyDirective) -> Self {
        self.directive = directive;
        self
    }

    /// Marks this accept as derived from conversational text rather than
    /// named explicitly.
    ///
    /// Changes nothing about how the task is profiled, scored, or run — only
    /// [`TopologyTurn::is_conclusive`] reads this, and only to decide whether
    /// an unusable answer may fall through to the conversational turn.
    #[must_use]
    pub const fn auto_routed(mut self) -> Self {
        self.origin = TaskOrigin::AutoRouted;
        self
    }

    /// Returns the accepted coding request.
    #[must_use]
    pub const fn task(&self) -> &CodingTask {
        &self.task
    }

    /// Returns the objective selection ranks this task under.
    #[must_use]
    pub const fn objective(&self) -> CodingObjective {
        self.objective
    }

    /// Returns the capabilities declared for this task's run context.
    #[must_use]
    pub const fn host_capabilities(&self) -> HostCapabilities {
        self.host
    }

    /// Returns what the user said about which topology should run this task.
    #[must_use]
    pub fn topology_directive(&self) -> &TopologyDirective {
        &self.directive
    }

    /// Returns where this accept came from.
    #[must_use]
    pub const fn origin(&self) -> TaskOrigin {
        self.origin
    }
}

/// The host's queue of accepted coding tasks, read once per turn by the
/// selection point.
///
/// Cloning shares one queue: the front end keeps a handle to
/// [`accept`](Self::accept) into, and [`CodingTaskQueuePlugin`] publishes the
/// same handle as a global resource for the run surface to
/// [`take`](Self::take) from.
///
/// Selection is *once per accepted coding task*: one turn takes at most one
/// entry, and an empty queue leaves the turn conversational.
#[derive(Debug, Clone, Default)]
pub struct CodingTaskQueue {
    accepted: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<AcceptedCodingTask>>>,
}

impl LocalResource for CodingTaskQueue {}
impl GlobalResource for CodingTaskQueue {}

impl Contract for CodingTaskQueue {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

impl CodingTaskQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepts one coding task for a future turn's selection point.
    pub fn accept(&self, accepted: AcceptedCodingTask) {
        if let Ok(mut queue) = self.accepted.lock() {
            queue.push_back(accepted);
        }
    }

    /// Removes and returns the next accepted coding task, if any.
    #[must_use]
    pub fn take(&self) -> Option<AcceptedCodingTask> {
        self.accepted.lock().ok()?.pop_front()
    }

    /// Returns how many coding tasks are waiting for a selection point.
    #[must_use]
    pub fn len(&self) -> usize {
        self.accepted.lock().map_or(0, |queue| queue.len())
    }

    /// Returns whether the next turn will be conversational for want of an
    /// accepted coding task.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Publishes a host-owned [`CodingTaskQueue`] so the run surface's selection
/// point can read it.
///
/// Add this plugin when the host wants live topology selection. Without it
/// (or with an empty queue) [`select_and_run`] is inert and every turn is the
/// ordinary conversational turn.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`CodingTaskQueue`] | Global | The accepted-coding-task queue the selection point drains, one entry per turn. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | The queue is consumed as a resource; the host keeps its own clone to accept into. |
///
/// # Dependencies
///
/// None of its own. Selection additionally needs a plugin providing
/// [`TopologyCatalog`] (normally
/// [`TopologyCatalogPlugin`](polaris_cosmos::topology::TopologyCatalogPlugin)),
/// at least one topology pack such as
/// [`CodingTopologyPackPlugin`](super::topology::CodingTopologyPackPlugin), and
/// a plugin providing [`TopologyEvidence`]; a task accepted without them is a
/// host misconfiguration and is reported as
/// [`TopologySelectionError::CapabilityMissing`].
///
/// # Lifecycle
///
/// - **`build()`** — inserts the supplied handle as a global resource.
/// - The plugin starts no task and accepts nothing on the host's behalf.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::cosmos::host::{CodingTaskQueue, CodingTaskQueuePlugin};
///
/// # async fn assemble() {
/// let queue = CodingTaskQueue::new();
/// let mut server = Server::new();
/// server.add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
/// server.finish().await.expect("the queue plugin resolves");
/// assert!(queue.is_empty());
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct CodingTaskQueuePlugin {
    queue: CodingTaskQueue,
}

impl CodingTaskQueuePlugin {
    /// Publishes the host's own queue handle.
    #[must_use]
    pub const fn new(queue: CodingTaskQueue) -> Self {
        Self { queue }
    }
}

#[plugin(
    id = "polaris_code::coding_task_queue",
    version = "0.1.0",
    provides(CodingTaskQueue)
)]
impl Plugin for CodingTaskQueuePlugin {
    fn build(&self, server: &mut Server) {
        assert!(
            server.insert_global(self.queue.clone()).is_none(),
            "CodingTaskQueue already has a global resource"
        );
    }
}

/// What the selection point decided, and what it did about it, for one turn.
///
/// Every variant is terminal for that turn: the run surface reports it and
/// returns control to the outer loop without running a second graph.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TopologyTurn {
    /// One registration was selected, readmitted live, and executed.
    Executed(Box<ExecutedTopology>),
    /// A model request was refused by the task's cost ceiling after the graph started.
    CostCapped(Box<CostCappedTopology>),
    /// Selection completed and no candidate passed every hard gate.
    ///
    /// The decision is persisted as a [`SelectionRecord`] before this is
    /// returned, and no graph — selected or default — runs in its place.
    NoEligibleCandidate(Box<NoTopologyStarted>),
    /// A candidate was selected, but neither it nor any captured pre-start
    /// fallback survived live readmission.
    ///
    /// Persisted as the selection record, every failed admission attempt, and
    /// a terminal [`SelectionNotStarted`].
    NotStarted(Box<NoTopologyStarted>),
    /// The user's own [`TopologyDirective::None`] refused a topology for this
    /// task before selection ran at all.
    ///
    /// No catalog read, no admission, no selection decision, nothing appended
    /// to evidence — there is nothing to select, because none was asked for.
    /// The workspace is untouched, same as every other variant here.
    Declined(Box<DeclinedWork>),
}

impl TopologyTurn {
    /// Returns the decision this turn's outcome belongs to, or `None` for
    /// [`Self::Declined`], which never reached a selection decision.
    #[must_use]
    pub fn decision_id(&self) -> Option<SelectionDecisionId> {
        match self {
            Self::Executed(run) => Some(run.decision_id),
            Self::CostCapped(run) => Some(run.decision_id),
            Self::NoEligibleCandidate(unstarted) | Self::NotStarted(unstarted) => {
                Some(unstarted.decision_id)
            }
            Self::Declined(_) => None,
        }
    }

    /// What this turn has to tell the user: the answer every front end shows
    /// where a conversational turn's answer goes.
    ///
    /// Total by construction, for every variant — an accepted coding task that
    /// reached a terminal answer is never allowed to end the turn silently.
    /// For [`Self::Executed`] the answer is the selected topology's own
    /// [`TopologyProduct`]; for the two no-start variants it is the selector's
    /// own exclusion codes, verbatim and in its order, named against the
    /// requested topology when there was one. This explains, it does not
    /// re-decide.
    #[must_use]
    pub fn answer(&self) -> String {
        match self {
            Self::Executed(run) => run.product.answer().unwrap_or_else(|| {
                // Unreachable: `execute_permitted` will not return a product
                // that accounts for nothing, so a started run cannot become a
                // blank turn here. Named rather than unwrapped so a future
                // change to that gate surfaces as a sentence, not a panic.
                "the selected coding topology reported nothing it assessed".to_owned()
            }),
            Self::CostCapped(run) => format!(
                "cost-cap: stopped `{}`; keeping the current workspace edits",
                run.registration_id
            ),
            Self::NoEligibleCandidate(unstarted) => match &unstarted.requested {
                Some(requested) => format!(
                    "you asked for `{requested}`; it could not be honoured ({})",
                    unstarted.reasons.join(", ")
                ),
                None => format!("no eligible candidate ({})", unstarted.reasons.join(", ")),
            },
            Self::NotStarted(unstarted) => {
                let mut answer = match &unstarted.requested {
                    Some(requested) => format!(
                        "you asked for `{requested}`; it did not survive live readmission ({})",
                        unstarted.reasons.join(", ")
                    ),
                    None => format!(
                        "no candidate survived live readmission ({})",
                        unstarted.reasons.join(", ")
                    ),
                };
                if unstarted.requested.is_none() && !unstarted.attempted.is_empty() {
                    answer.push_str(&format!("; attempted {}", unstarted.attempted.join(", ")));
                }
                answer
            }
            Self::Declined(declined) => format!(
                "no topology ran for `{}`: you asked for none",
                declined.task_id.id()
            ),
        }
    }

    /// Returns the registration that executed, or `None` when none started.
    #[must_use]
    pub fn started(&self) -> Option<&str> {
        match self {
            Self::Executed(run) => Some(run.registration_id.as_str()),
            Self::CostCapped(run) => Some(run.registration_id.as_str()),
            Self::NoEligibleCandidate(_) | Self::NotStarted(_) | Self::Declined(_) => None,
        }
    }

    /// Returns where the task this turn answers was accepted from.
    #[must_use]
    pub fn origin(&self) -> TaskOrigin {
        match self {
            Self::Executed(run) => run.origin,
            Self::CostCapped(run) => run.origin,
            Self::NoEligibleCandidate(unstarted) | Self::NotStarted(unstarted) => unstarted.origin,
            Self::Declined(declined) => declined.origin,
        }
    }

    /// Whether this turn reached a usable conclusion, as opposed to a
    /// terminal answer that only explains why it has nothing to say.
    ///
    /// This is the one place [`TaskOrigin`] changes what a turn's outcome
    /// means. For [`TaskOrigin::Explicit`] every variant here still ends the
    /// turn regardless of this predicate — that is the rule this module
    /// follows, and `select_and_run`'s caller does not consult
    /// this method for an explicit accept. For [`TaskOrigin::AutoRouted`],
    /// the caller uses this to decide whether the accepted task's answer is
    /// the turn's answer at all, or whether nobody having confirmed the
    /// routing means an inconclusive result should fall through to the
    /// ordinary conversational turn instead: [`Self::NoEligibleCandidate`]
    /// and [`Self::NotStarted`] are never conclusive, and
    /// [`Self::Executed`] is conclusive unless its product
    /// [`ConclusionKind::Escalated`] — the same status
    /// [`super::graphs::tree_question_graph`]'s own `answer_question` system
    /// uses for "this could not be answered", as opposed to "this was
    /// answered and the answer is that nothing matched". [`Self::Declined`] is
    /// always conclusive: it is only ever reached by an explicit
    /// `TopologyDirective::None` (`auto_route` never sets one), so it is a
    /// fully honoured request, not an unconfirmed guess that could need the
    /// fallthrough.
    #[must_use]
    pub fn is_conclusive(&self) -> bool {
        match self {
            Self::Executed(run) => run.product.kind != ConclusionKind::Escalated,
            Self::CostCapped(_) => true,
            Self::NoEligibleCandidate(_) | Self::NotStarted(_) => false,
            Self::Declined(_) => true,
        }
    }
}

/// A user directive of [`TopologyDirective::None`] declined a topology for
/// this accepted task.
#[derive(Debug, Clone)]
pub struct DeclinedWork {
    /// Host-owned task identity the directive was made for.
    pub task_id: TaskId,
    /// Where the task this decline answers was accepted from.
    ///
    /// `TopologyDirective::None` is only ever set by an explicit accept —
    /// `auto_route` never produces one — so this reads [`TaskOrigin::Explicit`]
    /// in practice; carried anyway so [`TopologyTurn::origin`] stays a total,
    /// record-derived read rather than an assumption.
    pub origin: TaskOrigin,
}

/// What a coding topology handed back, projected from the one output the
/// coding contract obliges every registration to produce.
///
/// # Why this is the floor
///
/// [`execute_permitted`] refuses to start a registration whose
/// [`GraphSignature`] does not `produce::<AssessmentConclusion>()`, so an
/// [`AssessmentConclusion`] is the *only* thing the host is entitled to assume
/// it can read back — and it can assume it of every registration, present and
/// future, on identical terms. Reading anything else (a registration's own
/// workspace, its change set, a pack-specific payload) would make the host
/// know one pack, and the next pack added would silently produce blank turns.
///
/// So this type is a projection of that conclusion and nothing more: the
/// derived [`ConclusionKind`], the [`StopReason`] the topology declared, and
/// the four independently visible status counts. It is deliberately plain
/// counts rather than a
/// [`StatusCounts`](polaris_cosmos::contracts::StatusCounts) so a host test
/// can construct the one case Cosmos's own constructors cannot produce — a
/// conclusion that accounted for nothing — and observe that it fails visibly
/// instead of rendering an empty answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologyProduct {
    /// Whether the topology resolved, left unresolved, or escalated.
    pub kind: ConclusionKind,
    /// Why the topology stopped.
    pub stop_reason: StopReason,
    /// Assessments that passed.
    pub pass: u64,
    /// Assessments that raised an issue.
    pub issue: u64,
    /// Assessments that did not apply.
    pub not_applicable: u64,
    /// Assessments that reached no verdict.
    pub indeterminate: u64,
}

impl TopologyProduct {
    /// Projects the terminal conclusion the selected graph produced.
    #[must_use]
    pub fn of(conclusion: &AssessmentConclusion) -> Self {
        let counts = conclusion.summary().counts_by_status();
        let only_not_applicable = counts.pass() == 0
            && counts.issue() == 0
            && counts.indeterminate() == 0
            && counts.not_applicable() > 0;
        Self {
            // Coding graphs turn this outcome into an issue or a real pass.
            // Keep this guard at the presentation boundary too: a future
            // registration can produce the shared contract without using
            // those graph helpers, and a verifier that made no applicable
            // claim must never be rendered as a successful coding task.
            kind: if only_not_applicable && conclusion.conclusion_kind() == ConclusionKind::Resolved
            {
                ConclusionKind::Unresolved
            } else {
                conclusion.conclusion_kind()
            },
            stop_reason: conclusion.stop_reason(),
            pass: counts.pass(),
            issue: counts.issue(),
            not_applicable: counts.not_applicable(),
            indeterminate: counts.indeterminate(),
        }
    }

    /// Projects a conclusion for `task`, refusing to present an edit task as
    /// resolved without graph-owned evidence that it applied an edit.
    ///
    /// A statement-only task asks the host to find and make an edit; unlike a
    /// read-only question or survey, a passing verifier alone does not show
    /// that work happened. The host accepts only an [`AppliedEdit`] from the
    /// graph that actually rewrote a file; test output, caches, and any other
    /// incidental workspace change do not satisfy this condition.
    #[must_use]
    pub fn for_task(
        conclusion: &AssessmentConclusion,
        task: &CodingTask,
        applied_edit: bool,
    ) -> Self {
        let mut product = Self::of(conclusion);
        if requires_change_evidence(task)
            && !applied_edit
            && product.kind == ConclusionKind::Resolved
        {
            product.kind = ConclusionKind::Unresolved;
            product.stop_reason = StopReason::Stagnated;
        }
        product
    }

    /// How many assessments this product accounts for.
    ///
    /// Saturating rather than wrapping: a count that overflowed `u64` is
    /// reported as "as many as can be said", never as zero.
    #[must_use]
    pub const fn assessed(self) -> u64 {
        self.pass
            .saturating_add(self.issue)
            .saturating_add(self.not_applicable)
            .saturating_add(self.indeterminate)
    }

    /// The answer this product is, or `None` when it accounts for nothing.
    ///
    /// `None` is the visible-failure floor: a registration that assessed
    /// nothing has nothing to say, and [`execute_permitted`] turns that into a
    /// [`TopologySelectionError::Execution`] rather than letting the turn
    /// render as an empty string.
    #[must_use]
    pub fn answer(self) -> Option<String> {
        if self.assessed() == 0 {
            return None;
        }
        let kind = if self.only_not_applicable() && self.kind == ConclusionKind::Resolved {
            // Preserve the same safety property for hand-built products in
            // tests and for future host projections that bypass `of`.
            ConclusionKind::Unresolved
        } else {
            self.kind
        };
        let headline = match kind {
            ConclusionKind::Resolved => "resolved the coding task",
            ConclusionKind::Unresolved => "left the coding task unresolved",
            ConclusionKind::Escalated => "escalated the coding task",
        };
        Some(format!(
            "{headline}: {} (stopped because {})",
            self.tally(),
            stop_phrase(self.stop_reason)
        ))
    }

    /// The non-zero status counts, in the order Cosmos declares them.
    fn tally(self) -> String {
        [
            (self.pass, "pass"),
            (self.issue, "issue"),
            (self.not_applicable, "not applicable"),
            (self.indeterminate, "indeterminate"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
    }

    /// Whether this product contains no verdict that established or rejected
    /// the task, only inapplicable checks.
    #[must_use]
    const fn only_not_applicable(self) -> bool {
        self.pass == 0 && self.issue == 0 && self.indeterminate == 0 && self.not_applicable > 0
    }
}

fn requires_change_evidence(task: &CodingTask) -> bool {
    task.edits().is_empty() && !task.is_survey() && !task.question().is_answerable()
}

/// The plain-words rendering of a [`StopReason`], for the one clause of the
/// answer that says why the topology stopped.
fn stop_phrase(reason: StopReason) -> &'static str {
    match reason {
        StopReason::GoalMet => "the declared goal was met",
        StopReason::PredicateSatisfied => "a stopping predicate held",
        StopReason::WorkExhausted => "no work remained",
        StopReason::IterationLimit => "the iteration limit was reached",
        StopReason::BudgetExhausted => "a budget was exhausted",
        StopReason::Saturated => "further work saturated",
        StopReason::Stagnated => "progress stagnated",
        StopReason::CircuitBroken => "a circuit breaker opened",
        StopReason::EscalatedOut => "the remaining work was escalated",
        StopReason::HumanStopped => "a human stopped it",
    }
}

/// The one registration a decision started, and what running it produced.
#[derive(Debug, Clone)]
pub struct ExecutedTopology {
    /// Decision that selected this registration.
    pub decision_id: SelectionDecisionId,
    /// Host-owned task identity the decision was made for.
    pub task_id: TaskId,
    /// Registration that ran.
    pub registration_id: String,
    /// Zero for the decision's primary choice, else the one-based position in
    /// its captured pre-start fallback plan.
    pub fallback_position: u64,
    /// Cosmos run identity the evidence episode is keyed by.
    pub cosmos_run_id: RunId,
    /// The Polaris executor's run identity — the trace join key.
    pub run_id: polaris::graph::RunId,
    /// How many graph nodes ran.
    pub nodes_executed: usize,
    /// Wall-clock duration of the graph execution.
    pub duration: Duration,
    /// What the topology concluded — the contract's own declared output,
    /// projected, and the material [`TopologyTurn::answer`] renders.
    pub product: TopologyProduct,
    /// What this run spent on its **own** meter.
    ///
    /// A topology run gets a fresh [`TokenMeter`] in its context, shadowing the
    /// session's. This is that meter, frozen after the graph returned, so a
    /// model-calling topology's own spend is broken out here as well as being
    /// folded onto the session's total. `None` when the session's model id
    /// could not be read, so an unmeasured run reports as unmeasured rather
    /// than as a free one.
    pub usage: Option<crate::usage::RunUsage>,
    /// Outcome observations this host's real verifiers appended afterwards.
    pub observations: Vec<OutcomeObservation>,
    /// The bounded Cosmos-block composition that was materialised for this
    /// run, including its native-validation witness and linked verifier
    /// outcome.  The ordinary topology evidence events remain the durable
    /// provider lane; this envelope makes their shared composition identity
    /// explicit to callers and replay tooling.
    pub composition: Option<polaris_cosmos::contracts::CompositionEvidence>,
    /// The policy request linked to [`Self::composition`]'s plan digest.
    /// This retains model stated grounds and prompt/response witnesses without
    /// pretending an opaque model response is a Cosmos graph record.
    pub composition_request: Option<composition::CompositionRequest>,
    /// Every gate-plan revision attempt this turn made before the one that
    /// ran, when [`Self::composition`] was assembled by [`super::planner`]
    /// rather than resolved from the finite recipe grammar. `None` for an
    /// ordinary finite-recipe composition, and for a gate-plan turn whose own
    /// revision loop exhausted before anything materialised — that fallback
    /// runs the finite recipe grammar instead, so [`Self::composition`] is a
    /// finite-recipe composition too, and this field stays `None` even
    /// though a revision loop did run first; see
    /// [`Self::exhausted_gate_plan_trail`] for that trail.
    pub gate_plan_trail: Option<polaris_cosmos::contracts::CompositionRevisionTrail>,
    /// Human-readable shape of the admitted gate plan, carried beside its
    /// durable digest for reports and externally graded plan history.
    pub gate_plan_shape: Option<String>,
    /// Every gate-plan revision attempt a candidate's own revision loop made
    /// before it exhausted and fell back to the finite recipe grammar for
    /// that same candidate, when that is what happened. Kept apart from
    /// [`Self::gate_plan_trail`] so a reader can never mistake an exhausted
    /// loop's own refused attempts for the composition that actually ran:
    /// every revision here is refused, [`Self::composition`] is always the
    /// finite-recipe composition that ran instead, and the two fields are
    /// never both `Some` at once.
    pub exhausted_gate_plan_trail: Option<polaris_cosmos::contracts::CompositionRevisionTrail>,
    /// The registration the user asked for, when this run was directed rather
    /// than the selector's own autonomous pick.
    ///
    /// This is the record [`TopologyDirective::Only`] and an autonomous run
    /// are told apart by: `Some(id)` means `id` ran because it was asked for,
    /// and `id == registration_id` always holds when this is `Some` — a
    /// directed decision forbids every other candidate, so nothing else could
    /// have won. `None` means the selector picked on its own.
    pub requested: Option<String>,
    /// `true` when [`requested`](Self::requested) is `Some` because of the
    /// operator's run-wide [`OperatorTopologyPin`], not this task's own
    /// `topology=` word. Always `false` when `requested` is `None`. See
    /// [`OperatorTopologyPin`] for why a reader needs this distinction: a
    /// pinned run and a task-directed run both forbid every other candidate
    /// the identical way, and only this field says which command did it.
    pub requested_by_pin: bool,
    /// Where the task this run answers was accepted from.
    pub origin: TaskOrigin,
    /// The complete candidate universe the decision considered, winner
    /// included, in the decision's own canonical order (by registration ID).
    ///
    /// Straight from [`SelectionDecision::candidates`] — nothing here is
    /// re-ranked, re-scored, or filtered. A renderer that wants "what else was
    /// considered" excludes `registration_id` itself from this list; it does
    /// not recompute anything to get there.
    pub candidates: Vec<CandidateConsideration>,
    /// Why the winner beat the runner-up, straight from the decision's own
    /// [`SelectionOutcome::Selected`] rationale.
    pub rationale: SelectionRationale,
}

/// A started task halted before a request that could cross its spend limit.
#[derive(Debug, Clone)]
pub struct CostCappedTopology {
    /// Selection decision that started the task.
    pub decision_id: SelectionDecisionId,
    /// Task whose workspace is retained.
    pub task_id: TaskId,
    /// Registration whose execution stopped.
    pub registration_id: String,
    /// Origin of the accepted task.
    pub origin: TaskOrigin,
    /// Task usage reported up to the refusal.
    pub usage: Option<crate::usage::RunUsage>,
}

/// A decision that reached a terminal answer without starting a graph.
#[derive(Debug, Clone)]
pub struct NoTopologyStarted {
    /// Decision that produced this answer.
    pub decision_id: SelectionDecisionId,
    /// Host-owned task identity the decision was made for.
    pub task_id: TaskId,
    /// Exclusion codes explaining the answer, in the order Cosmos reported
    /// them.
    ///
    /// When [`requested`](Self::requested) is `Some`, these are the requested
    /// candidate's own reasons — never the noise from every other candidate
    /// this decision also excluded to narrow the field to just that one.
    pub reasons: Vec<String>,
    /// Registrations whose live readmission was attempted and refused, in
    /// captured fallback order. Empty for
    /// [`TopologyTurn::NoEligibleCandidate`], where nothing was ever selected.
    pub attempted: Vec<String>,
    /// The registration the user asked for, when this refusal answers a
    /// [`TopologyDirective::Only`] rather than an autonomous decision that
    /// simply found nothing eligible.
    pub requested: Option<String>,
    /// `true` when [`requested`](Self::requested) is `Some` because of the
    /// operator's run-wide [`OperatorTopologyPin`] rather than this task's own
    /// directive. See [`ExecutedTopology::requested_by_pin`], the same
    /// distinction for the answer that did start a graph.
    pub requested_by_pin: bool,
    /// Where the task this answer was reached for was accepted from.
    pub origin: TaskOrigin,
}

/// Why the selection point could not reach a decision at all.
///
/// Distinct from a decision that answers "nothing is eligible": these are host
/// misconfigurations or rejected records, not selector verdicts, and the run
/// surface reports them as a failed turn rather than silently running the
/// conversational agent in the accepted task's place.
#[derive(Debug)]
#[non_exhaustive]
pub enum TopologySelectionError {
    /// A capability the accepted task needs was not installed on the server.
    CapabilityMissing(&'static str),
    /// The sealed catalog could not be read, or holds no coding contract.
    Catalog(String),
    /// The coding task or its profile was rejected.
    Task(super::task::CodingTaskError),
    /// Selector configuration or author priors were rejected.
    Policy(super::policy::PolicyError),
    /// A Cosmos record was rejected.
    Contract(polaris_cosmos::contracts::ContractError),
    /// Mechanical preflight or live readmission could not be built.
    Admission(String),
    /// Selection itself failed.
    Selection(String),
    /// The evidence store rejected an append or a statistics capture.
    Evidence(String),
    /// The selected graph was started and did not produce a conclusion.
    Execution(String),
}

impl std::fmt::Display for TopologySelectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapabilityMissing(capability) => write!(
                formatter,
                "a coding task was accepted but the host installed no {capability}"
            ),
            Self::Task(error) => error.fmt(formatter),
            Self::Policy(error) => error.fmt(formatter),
            Self::Contract(error) => error.fmt(formatter),
            Self::Catalog(detail)
            | Self::Admission(detail)
            | Self::Selection(detail)
            | Self::Evidence(detail)
            | Self::Execution(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for TopologySelectionError {}

impl From<super::task::CodingTaskError> for TopologySelectionError {
    fn from(error: super::task::CodingTaskError) -> Self {
        Self::Task(error)
    }
}

impl From<super::policy::PolicyError> for TopologySelectionError {
    fn from(error: super::policy::PolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<polaris_cosmos::contracts::ContractError> for TopologySelectionError {
    fn from(error: polaris_cosmos::contracts::ContractError) -> Self {
        Self::Contract(error)
    }
}

/// The test-runner capability this turn's availability probe, composition
/// facts, and live context all agree on.
///
/// A stated [`SuccessCheck::Command`] makes a runner installable, subject to
/// whatever shell permission policy the session already carries: a
/// [`ShellExecutor`] global present and returning anything other than
/// [`ShellPermission::Allow`] for the stated command line refuses the
/// capability rather than shelling out to a command the session's own policy
/// would not otherwise run — the same posture every other shell-invoking tool
/// in `src/tools/` already honours. No `ShellExecutor` global at all (a bare
/// test context, or a host that never installed [`ShellPlugin`](polaris::shell::ShellPlugin))
/// is read as no policy to honour, not as a denial. Any other success-check
/// kind leaves the host's own declared capability untouched — this function
/// only ever narrows `declared.test_runner` from `true` to `false`, never
/// widens it, and never touches `model_provider`.
fn effective_host_capabilities(
    task: &CodingTask,
    declared: HostCapabilities,
    globals: Option<&polaris::system::resource::Resources>,
) -> HostCapabilities {
    let test_runner = match task.success() {
        SuccessCheck::Command(command) => globals
            .and_then(|globals| globals.get::<polaris::shell::ShellExecutor>().ok())
            .is_none_or(|executor| {
                executor.check_permission(&command_line(command))
                    == polaris::shell::ShellPermission::Allow
            }),
        _ => declared.test_runner,
    };
    HostCapabilities {
        test_runner,
        model_provider: declared.model_provider,
    }
}

/// Renders a stated command and its arguments as one shell-syntax line, for
/// [`ShellExecutor::check_permission`](polaris::shell::ShellExecutor::check_permission)
/// alone — the runner itself never goes through a shell; see
/// [`ProjectTestRunner::judge`](super::payload::ProjectTestRunner::judge).
fn command_line(command: &super::task::SuccessCommand) -> String {
    let mut line = command.program().to_owned();
    for arg in command.args() {
        line.push(' ');
        line.push_str(arg);
    }
    line
}

/// Whether the session's shell policy would run
/// [`gate_blocks::reproduce`]'s own fixed reproduce-run command.
///
/// Checked against `ShellExecutor::check_permission` the same way
/// [`effective_host_capabilities`] checks a task's own *stated*
/// [`SuccessCheck::Command`] — the exact command line the reproduce block
/// will actually invoke, never a guess at "does this policy generally allow
/// test commands." The absent-executor default is deliberately the
/// *opposite* of `effective_host_capabilities`'s, though: that function reads
/// no `ShellExecutor` as "no policy to honour" and trusts an explicit,
/// caller-stated command, because refusing an ask nobody governed would be
/// the surprising choice. This function's answer is never asked for by
/// anyone — no caller named this command — so it has no ask to trust; a
/// hermetic host with no shell subsystem wired in at all must still mean *no
/// test runner*, which only [`ShellExecutor::check_permission`] actually
/// returning [`ShellPermission::Allow`](polaris::shell::ShellPermission::Allow)
/// establishes.
///
/// Deliberately a value separate from [`HostCapabilities::test_runner`]
/// rather than a wider [`effective_host_capabilities`] — see
/// [`planner::PlannerFacts::reproduce_runner_available`]'s own doc for why:
/// `test_runner`/`declared.test_runner` also feeds
/// [`topology::probe_availability`] (which registrations are even eligible)
/// and the finite-recipe/model composition policies for every task, not only
/// a statement-only one under [`composition::CompositionPolicy::Gates`], so
/// widening it here would change topology selection itself for stated-edit
/// and survey tasks too, in every composition policy — see
/// `docs/gate-planning.md`.
fn reproduce_runner_available(globals: Option<&polaris::system::resource::Resources>) -> bool {
    let command_line = {
        let mut line = gate_blocks::reproduce::REPRODUCE_SCRIPT_PROGRAM.to_owned();
        line.push(' ');
        line.push_str(gate_blocks::reproduce::REPRODUCE_SCRIPT_PATH);
        line
    };
    globals
        .and_then(|globals| globals.get::<polaris::shell::ShellExecutor>().ok())
        .is_some_and(|executor| {
            executor.check_permission(&command_line) == polaris::shell::ShellPermission::Allow
        })
}

/// Runs the ordered selection lifecycle for one turn of `session_id`.
///
/// Returns `Ok(None)` when no coding task was accepted for this turn, which is
/// the normal case: the caller then runs its ordinary conversational turn.
/// Returns `Ok(Some(_))` when a coding task was accepted and selection reached
/// a terminal answer — executed, no eligible candidate, or not started.
///
/// `turn_number` is the session's turn number *before* this turn. It is used
/// only to derive this turn's evidence event identities; it is never renumbered
/// and never replaced.
///
/// # Errors
///
/// Returns [`TopologySelectionError`] when a coding task was accepted but the
/// host cannot honour it: a missing catalog, pack, or evidence capability, or a
/// Cosmos record the contracts reject.
pub async fn select_and_run(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    turn_number: TurnNumber,
) -> Result<Option<TopologyTurn>, TopologySelectionError> {
    let Some(accepted) = take_accepted(sessions, session_id).await else {
        return Ok(None);
    };
    // `TopologyDirective::None` is answered before anything Cosmos-shaped
    // happens: no catalog read, no admission, no selection decision, nothing
    // appended to evidence. There is nothing to select, because none was
    // asked for, and the turn says so rather than ending silently.
    if *accepted.topology_directive() == TopologyDirective::None {
        return Ok(Some(TopologyTurn::Declined(Box::new(DeclinedWork {
            task_id: accepted.task().task_id(),
            origin: accepted.origin(),
        }))));
    }
    // The task's own word always wins when it has one; the operator's
    // run-wide pin only fills the silence of `Automatic` — see
    // `OperatorTopologyPin`'s own doc for why that precedence, not the
    // reverse. `requested_by_pin` is the provenance a later reader needs: both
    // sources produce the identical `Some(name)` `requested` carries, and
    // without this flag "the operator required this" and "the caller asked
    // for this" would be indistinguishable in the record.
    let (requested, requested_by_pin) = match accepted.topology_directive() {
        TopologyDirective::Only(name) => (Some(name.clone()), false),
        TopologyDirective::Automatic => match read_operator_pin(sessions, session_id).await {
            Some(pinned) => (Some(pinned), true),
            None => (None, false),
        },
        TopologyDirective::None => (None, false),
    };
    let ids = TurnEventIds::new(session_id, turn_number);
    let evidence = read_evidence(sessions, session_id).await.ok_or(
        TopologySelectionError::CapabilityMissing("TopologyEvidence capability"),
    )?;

    // The frozen inputs every step below shares. Reading the snapshot once is
    // what makes the decision, the live readmission, and the started run all
    // refer to the same catalog generation and digest.
    let frozen = read_catalog(sessions, session_id).await?;
    let task = accepted.task();
    let profile = task.profile()?;
    // Fetched once, ahead of the pristine run context: the availability probe
    // below and the live context built later both need to agree on whether a
    // stated command is actually permitted to run, and only one fetch of the
    // session's globals should ever answer that question for this turn.
    let globals = session_globals(sessions, session_id).await;
    let host_capabilities =
        effective_host_capabilities(task, accepted.host_capabilities(), globals.as_deref());
    // Unlike `host_capabilities` above, this fact gates a plan that actually
    // shells out to a model-authored script (`run_python_script`), so it
    // takes the opposite default: no `ShellExecutor` global reads as
    // unavailable, not as no policy to honour.
    let shell_available = globals
        .as_deref()
        .and_then(|globals| globals.get::<polaris::shell::ShellExecutor>().ok())
        .is_some_and(|executor| {
            let probe = format!(
                "python {}/self-check-probe.py",
                crate::state::STATE_DIR_NAME
            );
            executor.check_permission(&probe) == polaris::shell::ShellPermission::Allow
        });
    let reproduce_available = reproduce_runner_available(globals.as_deref());
    let availability = topology::probe_availability(
        task,
        ids.availability()?,
        unix_epoch_ms(),
        host_capabilities,
    )?;

    let admission = preflight_catalog(
        &frozen.snapshot,
        frozen.contract.key(),
        &availability,
        false,
    )
    .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;

    let statistics = evidence
        .capture_statistics(&policy::statistics_request())
        .map_err(|error| TopologySelectionError::Evidence(error.detail().to_owned()))?;
    let mut objective = accepted.objective().build()?;
    if let Some(name) = requested.as_deref() {
        objective = narrow_to(&frozen.snapshot, objective, name)?;
    }
    let decision_id = ids.decision()?;
    let decision = deterministic::select(&SelectionInputs::new(
        decision_id,
        profile.clone(),
        objective.clone(),
        frozen.contract.key().clone(),
        frozen.snapshot.clone(),
        availability.clone(),
        admission.clone(),
        statistics.clone(),
        // The same frozen snapshot the decision records as its statistics
        // input, read for what it says about each candidate under this task's
        // shape. Cold start — an empty snapshot — leaves the priors alone.
        policy::candidate_facts(&frozen.snapshot, Some((&statistics, &profile)))?,
        policy::selector_config()?,
    ))
    .map_err(|error| TopologySelectionError::Selection(error.to_string()))?;

    // Selection is evidence whether or not anything starts, so the record is
    // appended before the outcome is even inspected.
    let selection_record = SelectionRecord::new(
        RecordedAt::new(next_sequence(&evidence)?, Some(unix_epoch_ms())),
        decision.clone(),
        profile,
        objective,
    )?;
    let composition_recorded_at = selection_record.recorded_at();
    append(
        &evidence,
        EvidenceEvent::Selection(Box::new(selection_record)),
    )?;

    let (selected, fallback_plan, rationale) = match decision.outcome() {
        SelectionOutcome::NoEligibleCandidate { diagnostics } => {
            // A directed request reports the requested candidate's *own*
            // reasons, never the noise from every other candidate this
            // decision also excluded to narrow the field down to just that
            // one — see `narrow_to`.
            let reasons = match requested.as_deref() {
                Some(name) => directed_reasons(decision.candidates(), name),
                None => diagnostics
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect(),
            };
            return Ok(Some(TopologyTurn::NoEligibleCandidate(Box::new(
                NoTopologyStarted {
                    decision_id,
                    task_id: decision.task_id().clone(),
                    reasons,
                    attempted: Vec::new(),
                    requested: requested.clone(),
                    requested_by_pin,
                    origin: accepted.origin(),
                },
            ))));
        }
        SelectionOutcome::Selected {
            selected,
            fallback_plan,
            rationale,
            ..
        } => (
            selected.clone(),
            fallback_plan
                .iter()
                .map(|fallback| fallback.candidate().clone())
                .collect::<Vec<_>>(),
            rationale.clone(),
        ),
    };

    // Live context preparation is the embodiment's job. One pristine context
    // serves every pre-start attempt because a refused readmission leaves it
    // untouched — the permit's context-identity check is what ties the
    // admitted candidate to this exact object.
    //
    // Pristine means *locally* empty except for what this run needs; it does
    // not mean cut off from the host. The context is built on the session's
    // globals, so a graph can reach the shared capabilities the host wired —
    // the `ModelRegistry` above all, because a topology that cannot call a
    // model is a crippled graph, not a boundary. The boundary that does hold
    // is narrower and is enforced by what is inserted *locally*: a fresh
    // `TokenMeter` shadows the session's global one, so the run bills itself.
    let workspace = CodingWorkspace::for_task(task);
    let executor = GraphExecutor::new();
    let accounting = RunAccounting::open(sessions, session_id).await;
    let mut context = match globals.clone() {
        Some(globals) => SystemContext::with_globals(globals),
        None => SystemContext::new(),
    };
    context.insert(workspace.clone());
    context.insert(gate_blocks::configurable::ModelStepHandoff::default());
    context.insert(SweepWorklist::new(graphs::SWEEP_BUDGET));
    // Captured before the insert below shadows the slot, and folded back onto
    // this run's own meter when the run ends (see `SessionMeterMerge`), so a
    // model call this run makes still lands on the session's real spend.
    let _session_meter_merge = SessionMeterMerge {
        session: context.get_resource::<TokenMeter>().ok().map(|r| r.clone()),
        run: accounting.meter.clone(),
    };
    context.insert(accounting.meter.clone());
    // A live model step needs the same registry entry and model id the gate
    // planner reads below; when either is absent this stays unset, so a
    // graph that reaches for it fails live resource validation with a clear
    // missing-resource refusal rather than a panic. Computed as an owned
    // `Llm` handle so the borrow of `context` this reads does not outlive
    // the `insert` call right after it.
    let live_model_step = context
        .get_resource::<ModelRegistry>()
        .ok()
        .zip(accounting.model_id.as_deref())
        .and_then(|(registry, model_id)| registry.llm(model_id).ok());
    if let Some(llm) = live_model_step {
        // The same signal a system inside this graph would read via
        // `Res<RunCancellation>` — read here directly because installing the
        // executor happens before the graph runs, not from inside a system.
        // Absent, a directly-driven run (a test, the offline proof) is simply
        // not interruptible, matching `RunCancellation::never()`'s own default.
        let cancel = context
            .get_resource::<RunCancellation>()
            .ok()
            .map(|guard| RunCancellation::clone(&guard))
            .unwrap_or_else(RunCancellation::never);
        let handoff = context
            .get_resource::<gate_blocks::configurable::ModelStepHandoff>()
            .expect("task handoff was installed")
            .clone();
        let mut step = gate_blocks::configurable::LiveModelStep::new(llm)
            .with_handoff(handoff)
            .with_model_id(accounting.model_id.as_deref().expect("resolved model id"))
            .with_cancellation(cancel);
        let locate_model = crate::cosmos::planner::lean_locate_model_id();
        if let Ok(registry) = context.get_resource::<ModelRegistry>()
            && let Ok(cheap_llm) = registry.llm(&locate_model)
        {
            step = step.with_model(locate_model, cheap_llm);
        }
        // Lets a live propose step's `run_python` tool actually shell out,
        // subject to the same session shell policy every other tool honours;
        // absent, `run_python` calls fail closed (see `run_python_script`).
        if let Ok(shell) = context.get_resource::<polaris::shell::ShellExecutor>() {
            step = step.with_shell(shell.clone());
        }
        if let Ok(navigation) = context.get_resource::<crate::tools::code_graph::CodeNavigation>() {
            step = step.with_navigation(navigation.clone());
        }
        // Planned-graph model calls never reach the session transcript, so
        // they get their own, beside the session files, one per session.
        match crate::state::state_dir(task.working_dir()) {
            Ok(dir) => {
                step = step.with_transcript(
                    dir.join("model-steps")
                        .join(format!("{}.jsonl", session_id.as_str())),
                );
            }
            Err(error) => {
                tracing::warn!(%error, "model step calls will not be recorded");
            }
        }
        context.insert(gate_blocks::configurable::ModelStepProvider::new(step));
    }
    // Prepared unconditionally, like the worklist: the live context has to
    // satisfy whichever registration selection returned, and a task that
    // asked nothing carries an unanswerable question rather than no question.
    context.insert(task.question());
    // `code.coverage-audit`'s graph is `coverage_sweep::build`, whose own
    // first node reads these three resources rather than creating them —
    // they are private to `polaris_cosmos::cognition::approved`, so this is
    // the one public entry point that can seed them. Prepared unconditionally
    // for the same reason the worklist above is: harmless for any other
    // registration, since nothing else reads them.
    coverage_sweep::prepare_context::<EditSite>(
        &mut context,
        coverage_sweep::Config::new(
            graphs::SWEEP_BUDGET,
            graphs::SWEEP_BUDGET,
            graphs::SWEEP_BUDGET,
            StopReason::BudgetExhausted,
        )
        .map_err(|error| TopologySelectionError::Admission(error.to_string()))?,
    )
    .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
    // Installed only when the task stated a command and that command cleared
    // the same permission check `host_capabilities` already answered for the
    // availability probe above — a stated command a shell policy would not
    // run gets no runner, the same as no command stated at all: a plan
    // needing one is refused at live admission and the planner revises.
    if let SuccessCheck::Command(command) = task.success()
        && host_capabilities.test_runner
    {
        context.insert(super::payload::ProjectTestRunner::command(
            command.program().to_owned(),
            command.args().to_vec(),
        ));
    } else if reproduce_available
        && task.edits().is_empty()
        && !task.is_survey()
        && !matches!(task.intent(), CodingIntent::Answer)
    {
        // The statement-only shape `planner::scripted_plan` builds when
        // `PlannerFacts::reproduce_runner_available` is set
        // (`planner::model_reproduce_gated_expression`) names this runner —
        // never a task's own stated command, since a statement-only task
        // states none. Harmless for any other registration or plan shape,
        // like the worklist and coverage-sweep resources above: nothing else
        // reads it.
        context.insert(super::payload::ProjectTestRunner::command(
            gate_blocks::reproduce::REPRODUCE_SCRIPT_PROGRAM,
            [gate_blocks::reproduce::REPRODUCE_SCRIPT_PATH],
        ));
    }
    // Whoever is watching the session watches this graph too. Both reads are
    // absent-tolerant: a host with no front end and a host with no
    // instrumentation each run the graph exactly as they did before, and the
    // pristine context gains nothing the admission boundary can see (live
    // validation reports *missing* resources, never extra ones).
    attach_user_io(sessions, session_id, &mut context).await;
    let instrumentation = read_instrumentation(sessions, session_id).await;
    let composition_policy = read_composition_policy(sessions, session_id).await;
    let gate_planner_mode = read_gate_planner_mode(sessions, session_id).await;
    let domain_selection = read_domain_selection(sessions, session_id).await;
    // Built once per turn regardless of how many candidates are tried: a
    // sealed catalogue over static registrations, cheap to reseal, and never
    // mutated once built.
    let gate_catalogue = if domain_selection.arm == domains::Arm::Lean {
        gate_blocks::coding_block_catalogue_for_lean()
    } else {
        gate_blocks::coding_block_catalogue()
    };
    let gate_limits = read_gate_plan_limits(sessions, session_id).await;

    let mut attempted = Vec::new();
    let mut refusals = Vec::new();
    let mut attempt_ids = Vec::new();
    let mut started = None;
    let mut gate_plan_trail = None;
    let mut gate_plan_shape = None;
    let mut exhausted_gate_plan_trail = None;
    let mut model_chooser_calls: usize = 0;
    for (position, candidate) in std::iter::once(&selected)
        .chain(fallback_plan.iter())
        .enumerate()
    {
        // The model chooser is a real, billed call per candidate tried. An
        // ordinary refusal (bad JSON, an unapproved recipe, a provider
        // hiccup) must not stop this loop — see the per-candidate handling
        // below — so without a separate ceiling here, a catalog with many
        // eligible candidates could turn one accepted task into one real
        // model call per registration. Capping the count of real calls,
        // rather than the count of candidates tried, leaves a
        // cheaply-refused deterministic-only candidate free to keep walking
        // the fallback plan.
        if matches!(composition_policy, composition::CompositionPolicy::Model) {
            if model_chooser_calls >= composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN {
                refusals.push(chooser_budget_exhausted_reason(candidate)?);
                break;
            }
            model_chooser_calls += 1;
        }
        // The selected registration is a catalog-approved coding shape.  Its
        // composition recipe names the Cosmos blocks that build an equivalent
        // fresh native graph for this turn.  A direction is provenance only:
        // catalog preflight and live admission below remain the authority.
        let template =
            read_composition_template(sessions, session_id, candidate.registration_id().as_str())
                .await;
        let facts = composition::CompositionFacts {
            registration: candidate.registration_id().as_str().to_owned(),
            scope_size: task.scope().len(),
            success_check: match task.success() {
                SuccessCheck::StatedEdits { .. } => "stated-edits",
                SuccessCheck::Command(_) => "command",
                SuccessCheck::Delegated { .. } => "delegated",
            },
            objective: match task.intent() {
                CodingIntent::Fast => "fast",
                CodingIntent::Careful => "careful",
                CodingIntent::Thorough => "thorough",
                CodingIntent::Answer => "answer",
            },
            latency_budget_ms: task.latency_budget().map(|budget| budget.get()),
            test_runner_available: host_capabilities.test_runner,
            model_provider_available: host_capabilities.model_provider,
        };

        // A task-specific plan takes the place of a whole-graph recipe
        // choice for this candidate. A materialised plan is admitted and run
        // exactly like a finite-recipe composition below; a revision loop
        // that exhausted every attempt falls through to the ordinary finite
        // recipe grammar for this same candidate instead of trying the next
        // one, since nothing about the candidate itself was refused.
        if matches!(composition_policy, composition::CompositionPolicy::Gates) {
            let planner_facts = planner::PlannerFacts::from_composition_facts(
                facts.clone(),
                matches!(task.intent(), CodingIntent::Answer),
                task.is_survey(),
                !task.edits().is_empty(),
                reproduce_available,
                task.statement(),
                shell_available,
            );
            let history = plan_history::render_plan_history();
            let attempts = planner::attempt_gate_plan(
                gate_planner_mode,
                domain_selection,
                &planner_facts,
                &gate_catalogue,
                &gate_limits.0,
                &frozen.snapshot,
                frozen.contract.key(),
                context.get_resource::<ModelRegistry>().ok().as_deref(),
                accounting.model_id.as_deref(),
                &accounting.meter,
                &mut model_chooser_calls,
                history.as_deref(),
            )
            .await
            .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;

            let contract_error = |error: polaris_cosmos::contracts::ContractError| {
                TopologySelectionError::Admission(error.to_string())
            };

            // A refused attempt's composition evidence never reaches the
            // real evidence store: materialisation refuses before a graph
            // exists, so there is nothing to submit to live admission and
            // nothing to append. Nothing ever checks a refused pair's
            // sequence against the store's own position — only the strict
            // order among these records themselves is checked — so every
            // pair in this trail is numbered from one small counter kept
            // entirely apart from the store, never a prediction of where a
            // later real event will land. That includes the admitted pair:
            // the real store's own position at admission time can be as low
            // as two on a fresh turn regardless of how many attempts were
            // refused first (a refusal never advances the store), so reusing
            // it would leave no room below it for whatever local revisions
            // came first. The admitted pair, and the run and outcome that
            // close out the same trail, continue this counter instead of
            // reusing any real position — unlike the finite-recipe path
            // below, which has no revision loop to make room for and reuses
            // its own real admission position throughout.
            let mut local_sequence: u64 = 1;
            let mut revisions = Vec::new();
            let mut materialized = None;
            // Reset for this candidate: a stale trail from an earlier
            // candidate whose own exhausted fallback then failed must never
            // ride along onto a later candidate's unrelated result.
            exhausted_gate_plan_trail = None;
            for result in attempts {
                match result {
                    planner::GatePlanAttemptResult::Materialized {
                        choice,
                        record,
                        attempt,
                        graph,
                        native_signature,
                        shape_label,
                    } => {
                        materialized = Some((
                            choice,
                            record,
                            attempt,
                            graph,
                            native_signature,
                            shape_label,
                        ));
                        break;
                    }
                    planner::GatePlanAttemptResult::Refused {
                        choice,
                        record,
                        attempt,
                        stage,
                        refusals: attempt_refusals,
                    } => {
                        let decision_recorded_at =
                            RecordedAt::new(LogicalSequence::new(local_sequence), None);
                        local_sequence += 1;
                        let decision = CompositionDecision::new(
                            decision_id,
                            decision_recorded_at,
                            choice,
                            vec![provenance(
                                SourceKind::CosmosPolicy,
                                EpistemicBasis::Verified,
                            )],
                        )
                        .map_err(contract_error)?;
                        let decision = decision
                            .with_gate_plan(record, attempt)
                            .map_err(contract_error)?;
                        let validation_recorded_at =
                            RecordedAt::new(LogicalSequence::new(local_sequence), None);
                        local_sequence += 1;
                        let attempt_id = ids.gate_plan_attempt(position, attempt)?;
                        // No graph was built, so there is no real native
                        // signature to witness; the empty digest marks this
                        // validation as one that never reached native
                        // validation at all.
                        let placeholder_witness = NativeGraphSignatureWitness::new(
                            frozen.contract.key().clone(),
                            ContentDigest::sha256(&[]),
                        );
                        let reason = ExclusionCode::new(
                            attempt_refusals
                                .first()
                                .map_or("plan.refused", |refusal| refusal.code()),
                        )
                        .map_err(contract_error)?;
                        let validation = CompositionValidation::refused(
                            attempt_id,
                            &decision,
                            validation_recorded_at,
                            placeholder_witness,
                            stage,
                            reason,
                            attempt_refusals,
                        )
                        .map_err(contract_error)?;
                        revisions.push(
                            CompositionRevisionEvidence::new(decision, validation)
                                .map_err(contract_error)?,
                        );
                    }
                }
            }

            if let Some((choice, record, attempt, graph, native_signature, shape)) = materialized {
                executor
                    .validate_resources(&graph, &context, None)
                    .map_err(|error| {
                        TopologySelectionError::Admission(format!(
                            "gate-plan composition for '{}' failed live resource validation: \
                             {error:?}",
                            candidate.registration_id().as_str()
                        ))
                    })?;
                let attempt_id = ids.admission(position)?;
                attempt_ids.push(attempt_id);
                attempted.push(candidate.registration_id().as_str().to_owned());
                // The evidence store's own episode checks every admission
                // against the decision's recorded candidate set, so this
                // stays the real catalog candidate the selection decided
                // among — exactly what the finite-recipe path below
                // passes to `attempt_live_admission` — even though the
                // graph it is admitting came from a gate plan rather
                // than that candidate's registered recipe.
                let (evaluation, permit) = attempt_gate_plan_live_admission(
                    &frozen,
                    &availability,
                    &context,
                    &executor,
                    candidate,
                    decision_id,
                    attempt_id,
                    next_sequence(&evidence)?,
                    &graph,
                )?;
                let admitted = evaluation.is_admitted();
                for reason in evaluation.result().reasons() {
                    refusals.push(reason.clone());
                }
                append(&evidence, EvidenceEvent::Admission(evaluation.clone()))?;
                if admitted && let Some(permitted) = permit {
                    // The real store's own position at admission time can be
                    // as low as two on a fresh turn (one candidate, one
                    // admission, nothing else appended) regardless of how
                    // many gate-plan attempts were refused first, since a
                    // refusal never touches the store. Reusing that real
                    // position here would leave no room below it for
                    // whatever local revisions came first, so the admitted
                    // pair continues the same local counter every earlier
                    // revision in this trail used, rather than reusing the
                    // real admission position at all.
                    let decision_recorded_at =
                        RecordedAt::new(LogicalSequence::new(local_sequence), None);
                    local_sequence += 1;
                    let decision = CompositionDecision::new(
                        decision_id,
                        decision_recorded_at,
                        choice,
                        vec![provenance(
                            SourceKind::CosmosPolicy,
                            EpistemicBasis::Verified,
                        )],
                    )
                    .map_err(contract_error)?
                    .with_gate_plan(record, attempt)
                    .map_err(contract_error)?;
                    let gate_composition = composition::MaterializedComposition::from_gate_plan(
                        decision,
                        *graph,
                        native_signature,
                        planner::GATE_PLAN_RECIPE.to_owned(),
                        format!(
                            "{} gate planner, revision attempt {}",
                            gate_planner_mode.as_str(),
                            attempt.get()
                        ),
                        None,
                        None,
                    );
                    let validation_recorded_at =
                        RecordedAt::new(LogicalSequence::new(local_sequence), None);
                    local_sequence += 1;
                    let final_validation = gate_composition
                        .admitted_validation(attempt_id, validation_recorded_at)
                        .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
                    let final_revision = CompositionRevisionEvidence::new(
                        gate_composition.decision().clone(),
                        final_validation,
                    )
                    .map_err(contract_error)?;
                    revisions.push(final_revision);
                    let trail = CompositionRevisionTrail::new(revisions, None, None)
                        .map_err(contract_error)?;
                    gate_plan_trail = Some(trail);
                    gate_plan_shape = Some(shape);
                    // The run and outcome that close out this same trail
                    // continue the identical local counter: the real store
                    // positions `execute_permitted` would otherwise reuse
                    // here can easily sit below these local revision
                    // numbers, since none of them ever touched the store.
                    let run_recorded_at =
                        RecordedAt::new(LogicalSequence::new(local_sequence), None);
                    local_sequence += 1;
                    let outcome_recorded_at =
                        RecordedAt::new(LogicalSequence::new(local_sequence), None);
                    started = Some((
                        evaluation,
                        permitted,
                        u64::try_from(position).unwrap_or(u64::MAX),
                        gate_composition,
                        validation_recorded_at,
                        Some((run_recorded_at, outcome_recorded_at)),
                    ));
                    break;
                }
                continue;
            }

            tracing::debug!(
                attempts = revisions.len(),
                registration = candidate.registration_id().as_str(),
                "gate-plan revision loop exhausted; falling back to the finite recipe \
                 grammar for this candidate",
            );
            // Every attempt this candidate's revision loop made was refused,
            // but the audit trail of that loop should not vanish just
            // because the finite recipe grammar is what actually ran: kept
            // apart from `gate_plan_trail` (which names an admitted gate
            // plan) so a reader can never mistake this for the composition
            // that executed.
            if !revisions.is_empty() {
                exhausted_gate_plan_trail = Some(
                    CompositionRevisionTrail::new(revisions, None, None)
                        .map_err(|error| TopologySelectionError::Admission(error.to_string()))?,
                );
            }
        }

        // An ordinary chooser failure — bad JSON, an unapproved recipe, an
        // out-of-bounds worklist bound, empty grounds, or a provider hiccup —
        // is exactly the kind of per-attempt refusal live admission already
        // recovers from below: it refuses *this* candidate and lets the loop
        // try the next fallback, rather than aborting `select_and_run` and
        // taking the whole turn down with it (`TopologySelectionError::Admission`
        // there ends in `DriverStep::Stop`, which a routine model quirk must
        // never reach). Recorded exactly like a live-admission refusal: an
        // `AdmissionEvaluation` appended immediately, so the call the meter
        // just billed is not a discard even when it is not the winner.
        // A gate-plan revision loop that exhausted every attempt falls
        // through to here, but `Gates` itself names no finite-recipe grammar
        // member — it is not one of `choose`'s two policies at all — so the
        // fallback resolves deterministically rather than passing `Gates`
        // through and having `choose` mistake it for the model policy.
        let fallback_policy = match &composition_policy {
            composition::CompositionPolicy::Gates => composition::CompositionPolicy::Deterministic,
            other => (*other).clone(),
        };
        let request = match composition::choose(
            fallback_policy,
            &facts,
            template,
            context.get_resource::<ModelRegistry>().ok().as_deref(),
            accounting.model_id.as_deref(),
            &accounting.meter,
        )
        .await
        {
            Ok(request) => request,
            Err(error) => {
                let attempt_id = ids.admission(position)?;
                attempt_ids.push(attempt_id);
                attempted.push(candidate.registration_id().as_str().to_owned());
                let evaluation = chooser_refusal_evaluation(
                    candidate,
                    &frozen,
                    &availability,
                    decision_id,
                    attempt_id,
                    RecordedAt::new(next_sequence(&evidence)?, Some(unix_epoch_ms())),
                    &error,
                )?;
                for reason in evaluation.result().reasons() {
                    refusals.push(reason.clone());
                }
                append(&evidence, EvidenceEvent::Admission(evaluation))?;
                continue;
            }
        };
        if let Some(bound) = request.worklist_bound() {
            context.insert(SweepWorklist::new(bound));
        }
        let composition = composition::select_and_materialize_request(
            candidate.registration_id().as_str(),
            template,
            request,
            decision_id,
            composition_recorded_at,
            requested.is_some(),
            frozen.contract.key(),
        )
        .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
        // The catalog admission below validates its frozen registered
        // counterpart and issues the one-use permit.  Validate resource
        // bindings against this turn's fresh graph as well, so no graph can
        // start merely because an older registered instance happened to be
        // compatible with the context.
        executor
            .validate_resources(composition.graph(), &context, None)
            .map_err(|error| {
                TopologySelectionError::Admission(format!(
                    "materialised composition '{}' failed live resource validation: {error:?}",
                    candidate.registration_id().as_str()
                ))
            })?;
        let attempt_id = ids.admission(position)?;
        attempt_ids.push(attempt_id);
        attempted.push(candidate.registration_id().as_str().to_owned());
        let attempt = attempt_live_admission(
            sessions,
            session_id,
            &frozen,
            &availability,
            &context,
            &executor,
            candidate,
            decision_id,
            attempt_id,
            next_sequence(&evidence)?,
        )
        .await?;
        let (evaluation, permit) = attempt;
        let admitted = evaluation.is_admitted();
        for reason in evaluation.result().reasons() {
            refusals.push(reason.clone());
        }
        append(&evidence, EvidenceEvent::Admission(evaluation.clone()))?;
        if admitted && let Some(permitted) = permit {
            let validation_recorded_at = evaluation.recorded_at();
            started = Some((
                evaluation,
                permitted,
                u64::try_from(position).unwrap_or(u64::MAX),
                composition,
                validation_recorded_at,
                None,
            ));
            break;
        }
    }

    let Some((
        admission_event,
        permitted,
        fallback_position,
        composition,
        admitted_validation_recorded_at,
        local_composition_tail,
    )) = started
    else {
        let not_started = SelectionNotStarted::new(
            decision_id,
            RecordedAt::new(next_sequence(&evidence)?, Some(unix_epoch_ms())),
            attempt_ids,
            refusals.clone(),
        )?;
        append(&evidence, EvidenceEvent::SelectionNotStarted(not_started))?;
        return Ok(Some(TopologyTurn::NotStarted(Box::new(
            NoTopologyStarted {
                decision_id,
                task_id: decision.task_id().clone(),
                reasons: refusals
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect(),
                attempted,
                requested: requested.clone(),
                requested_by_pin,
                origin: accepted.origin(),
            },
        ))));
    };

    let executed = execute_permitted(
        &evidence,
        frozen.contract.key(),
        &ids,
        &decision,
        &admission_event,
        admitted_validation_recorded_at,
        &permitted,
        composition,
        fallback_position,
        rationale,
        &executor,
        &mut context,
        &workspace,
        &accounting,
        accepted.host_capabilities(),
        task,
        instrumentation
            .as_ref()
            .map(GraphInstrumentation::middleware),
        requested,
        requested_by_pin,
        local_composition_tail,
    )
    .await;
    if executed.is_err() && accounting.meter.cost_cap_stopped() {
        return Ok(Some(TopologyTurn::CostCapped(Box::new(
            CostCappedTopology {
                decision_id: decision.decision_id(),
                task_id: task.task_id(),
                registration_id: permitted.registration_id().as_str().to_owned(),
                origin: accepted.origin(),
                usage: accounting.snapshot(),
            },
        ))));
    }
    let executed = executed?;
    let executed = ExecutedTopology {
        origin: accepted.origin(),
        gate_plan_trail,
        gate_plan_shape,
        exhausted_gate_plan_trail,
        ..executed
    };
    Ok(Some(TopologyTurn::Executed(Box::new(executed))))
}

/// Narrows `objective` to admit only `only`, by forbidding every other
/// registration's own capability tag.
///
/// Composed entirely from existing primitives:
/// [`policy::forbid_all_but`] reads the sealed catalog snapshot for the tags
/// to forbid, and [`HardConstraints::with_forbidden_capabilities`] is the same
/// hard-policy gate Cosmos already exposes — the one
/// [`CodingIntent::Answer`](super::task::CodingIntent::Answer) already uses to
/// exclude every source-mutating shape. Mechanical admission is untouched: it
/// ran before this is ever called, over the whole catalog, so a registration
/// admission already refused stays refused regardless of this narrowing — a
/// user's request can only restrict which admitted candidates are considered,
/// never admit past what admission decided.
///
/// # Errors
///
/// Returns an error if a composed capability name or the rebuilt objective is
/// rejected.
///
/// [`HardConstraints::with_forbidden_capabilities`]: polaris_cosmos::contracts::HardConstraints::with_forbidden_capabilities
fn narrow_to(
    catalog: &TopologyCatalogSnapshot,
    objective: Objective,
    only: &str,
) -> Result<Objective, TopologySelectionError> {
    let mut forbidden = objective.hard().forbidden_capabilities().to_vec();
    forbidden.extend(policy::forbid_all_but(catalog, only)?);
    let hard = objective
        .hard()
        .clone()
        .with_forbidden_capabilities(forbidden);
    Ok(Objective::new(
        hard,
        objective.preferences().clone(),
        objective.missing_preference_evidence(),
        objective.tie_break().clone(),
        objective.provenance().to_vec(),
    )?)
}

/// Returns `requested`'s own exclusion reasons from a decision's considered
/// candidates — never the aggregate noise from every other candidate a
/// directed decision also excluded to narrow the field down to just that one.
///
/// Falls back to a `topology.unknown` reason when `requested` names nothing
/// this decision considered at all. That should not happen through the
/// accepting affordance, which validates the name against the same sealed
/// catalog before anything is queued; this is the defensive answer for a
/// directive built directly through the public
/// [`AcceptedCodingTask`] constructor rather than through it.
fn directed_reasons(candidates: &[CandidateConsideration], requested: &str) -> Vec<String> {
    let Some(candidate) = candidates
        .iter()
        .find(|candidate| candidate.candidate().registration_id().as_str() == requested)
    else {
        return vec!["topology.unknown".to_owned()];
    };
    if !candidate.mechanical_admission().is_admitted() {
        return candidate
            .mechanical_admission()
            .reasons()
            .iter()
            .map(|reason| reason.code().as_str().to_owned())
            .collect();
    }
    candidate
        .policy_eligibility()
        .reasons()
        .iter()
        .map(|reason| reason.code().as_str().to_owned())
        .collect()
}

/// The fixed reference every [`auto_route`] accept is recorded under.
///
/// A stable constant rather than one minted per turn: nothing here reads two
/// auto-routed tasks' identities as distinct, `Cosmos` stable-name rules
/// forbid deriving one from the (arbitrary, user-authored) question text
/// itself, and existing host tests already reuse a fixed reference string
/// across repeated accepts without needing per-call uniqueness.
const AUTO_ROUTE_REFERENCE: &str = "auto-answer";

/// Classifies conversational text as a light-effort question and, if it is
/// one, accepts it the same way an explicit `--topology-task` accept would —
/// as an [`AcceptedCodingTask`], marked [`TaskOrigin::AutoRouted`].
///
/// Returns `None` for anything [`CodingQuestion::parse`] does not recognise,
/// so an ordinary conversational turn is never even profiled: classification
/// is a lexical prefix match over `text`, not a model call, and costs nothing
/// a turn that needs full treatment would notice.
///
/// The accepted task's [`SuccessCheck`] is [`SuccessCheck::delegated`]: this
/// is a question, not an edit, so there is no independent success predicate
/// to check — the verdict *is* the topology's own
/// [`AssessmentConclusion::conclusion_kind`], read back through
/// [`TopologyTurn::is_conclusive`].
///
/// # Errors
///
/// Returns `None` (rather than propagating) when `working_dir` is not an
/// existing directory or the recognised text is blank after trimming its
/// matched prefix — both mean routing is unavailable, not that the turn
/// failed, so the caller's fallback is simply to run the conversational turn
/// as it always has.
#[must_use]
pub fn auto_route(working_dir: &std::path::Path, text: &str) -> Option<AcceptedCodingTask> {
    let question = CodingQuestion::parse(text);
    if !question.is_answerable() {
        return None;
    }
    let success = SuccessCheck::delegated("polaris-code.auto-route").ok()?;
    let task = CodingTask::from_statement(
        AUTO_ROUTE_REFERENCE,
        working_dir,
        text,
        CodingIntent::Answer,
        success,
    )
    .ok()?
    .with_question(text);
    Some(AcceptedCodingTask::new(task).auto_routed())
}

/// The catalog values every step of one decision shares.
///
/// Read once, so the decision, the live readmission, and the started run all
/// refer to the same generation and digest rather than to three reads that
/// could disagree.
struct FrozenCatalog {
    snapshot: TopologyCatalogSnapshot,
    contract: TopologyContract,
    known_schemas: BTreeSet<ConfigurationSchemaKey>,
}

/// One topology run's own accounting: a private meter and the model id its
/// snapshot is reported under.
///
/// The meter is inserted into the run context as a local resource, where it
/// shadows the session's global [`TokenMeter`]. A graph that reads
/// `Res<TokenMeter>` therefore reaches this one and never the session's, which
/// is the whole mechanism behind "a topology has its own meter".
///
/// The model id is the session's own configured model, read from
/// [`AgentConfig`](crate::agents::AgentConfig) the same way
/// [`crate::run`] reads it, so a run's cost estimate is derived from the model
/// the host actually configured rather than from a guess. `None` when the
/// session cannot be addressed, which reports an unmeasured run as unmeasured
/// rather than as a run that cost nothing.
struct RunAccounting {
    meter: TokenMeter,
    model_id: Option<String>,
}

impl RunAccounting {
    /// Opens a fresh meter for one run and reads the session's model id.
    async fn open(sessions: &SessionsAPI, session_id: &SessionId) -> Self {
        let (model_id, cost_limit) = sessions
            .with_context(session_id, |ctx| {
                (
                    ctx.get_resource::<AgentConfig>()
                        .ok()
                        .map(|config| config.model_id.clone()),
                    ctx.get_resource::<TaskCostLimit>()
                        .ok()
                        .map(|limit| limit.0),
                )
            })
            .await
            .unwrap_or((None, None));
        let meter = TokenMeter::new();
        if let Some(limit) = cost_limit {
            meter
                .set_task_cost_cap(limit)
                .expect("task cost resource is validated");
        }
        Self { meter, model_id }
    }

    /// Freezes the run's own meter, or `None` when the model id is unknown.
    fn snapshot(&self) -> Option<crate::usage::RunUsage> {
        self.model_id
            .as_ref()
            .map(|model_id| self.meter.snapshot(model_id))
    }
}

/// Folds a run's own billed spend onto the session's real meter when the run
/// ends, however it ends — normal completion, an early refusal, or an
/// early `?` return all drop this the same way, so no exit path has to
/// remember to do it.
///
/// The run's own [`TokenMeter`] is a fresh, locally-inserted resource (see
/// [`RunAccounting`]) that shadows the session's global one, so nothing
/// billed against it would otherwise reach the session's usage report or its
/// spend ceiling. `session` is `None` when the session's global meter could
/// not be read, in which case there is nothing to fold onto.
struct SessionMeterMerge {
    session: Option<TokenMeter>,
    run: TokenMeter,
}

impl Drop for SessionMeterMerge {
    fn drop(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        session.merge_from(&self.run);
    }
}

/// Clones the session's global resource table so a run context can reach the
/// capabilities the host wired for the session.
///
/// Returns `None` for a session that cannot be addressed or that was built
/// without globals; the caller then falls back to a context with none, which
/// is what every graph reading only its own locally inserted resources needs.
async fn session_globals(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> Option<std::sync::Arc<polaris::system::resource::Resources>> {
    sessions
        .with_context(session_id, |ctx| ctx.globals_arc())
        .await
        .ok()
        .flatten()
}

/// Takes at most one accepted coding task out of the session's queue.
///
/// A session with no [`CodingTaskQueue`] — or one that cannot be addressed —
/// yields `None`, which leaves the turn conversational.
async fn take_accepted(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> Option<AcceptedCodingTask> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<CodingTaskQueue>()
                .ok()
                .and_then(|queue| queue.take())
        })
        .await
        .ok()
        .flatten()
}

/// Gives the pristine run context its own [`UserIO`], built from the same
/// factory — and therefore the same provider — the session's front end
/// registered.
///
/// This is what makes a topology run watchable: the instrumentation around a
/// node reports to whatever `UserIO` the executing context resolves, and the
/// host's context is built from scratch rather than descended from the
/// session's. `UserIO` is not cloneable, so a fresh instance over the shared
/// provider is the composition that reaches the same panel.
///
/// A host that registered no `UserIO` factory — headless with no IO plugin, a
/// test, an embedding with no front end — is left exactly as it was. Nothing is
/// inserted, nothing is logged, and the graph runs unchanged.
async fn attach_user_io(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    context: &mut SystemContext<'static>,
) {
    let factory = sessions
        .with_context(session_id, |ctx| {
            ctx.factory_fn_by_type_id(TypeId::of::<UserIO>())
        })
        .await
        .ok()
        .flatten();
    if let Some(factory) = factory {
        // Retain the factory on the entry so a scope node inside a topology
        // graph can `forward_fresh::<UserIO>()` the same way the agent's can.
        context.insert_boxed_with_factory(TypeId::of::<UserIO>(), factory.produce(), factory);
    }
}

/// Clones the app's graph middleware chain out of the session's resources.
///
/// `None` means no plugin published one, which is the same answer as "nobody
/// asked to watch": the run then executes with the executor's default empty
/// chain, byte for byte as it did before instrumentation existed.
async fn read_instrumentation(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> Option<GraphInstrumentation> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<GraphInstrumentation>()
                .ok()
                .map(|handle| GraphInstrumentation::clone(&handle))
        })
        .await
        .ok()
        .flatten()
}

/// Reads the operator's run-wide topology pin, if `main.rs` installed one.
///
/// `None` for a run with no [`OperatorTopologyPin`] global at all — today's
/// behaviour, unchanged — and for a host built without one, such as the test
/// harnesses in this crate's own test suite that never insert it.
async fn read_operator_pin(sessions: &SessionsAPI, session_id: &SessionId) -> Option<String> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<OperatorTopologyPin>()
                .ok()
                .map(|pin| pin.registration_id().to_owned())
        })
        .await
        .ok()
        .flatten()
}

/// Clones the host's evidence facade out of the session's resource hierarchy.
async fn read_evidence(sessions: &SessionsAPI, session_id: &SessionId) -> Option<TopologyEvidence> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<TopologyEvidence>()
                .ok()
                .map(|evidence| TopologyEvidence::clone(&evidence))
        })
        .await
        .ok()
        .flatten()
}

/// Reads an optional application-pack composition template for `registration`.
///
/// The shipped pack's finite templates live in [`composition`] itself.  An
/// application pack that owns an additional catalog registration can publish
/// this tiny startup resource so its graph is still rebuilt and evidenced as a
/// finite composition rather than being an invisible special case.
async fn read_composition_template(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    registration: &str,
) -> Option<composition::CompositionTemplate> {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<composition::CompositionTemplates>()
                .ok()
                .and_then(|templates| templates.template(registration))
        })
        .await
        .ok()
        .flatten()
}

/// Reads the explicitly installed composition policy.  No resource means the
/// deterministic policy; model selection is therefore opt-in at composition
/// root rather than an accidental consequence of a provider being available.
///
/// This reads only the resource, never `POLARIS_CODE_COMPOSITION_POLICY`
/// itself — exactly like [`read_operator_pin`] reads only
/// [`OperatorTopologyPin`], never `POLARIS_CODE_TOPOLOGY_PIN`. `main.rs`
/// reads, validates and installs both env vars once at startup, before any
/// turn, so a typo refuses the process rather than this per-turn read
/// discovering it deep inside a selection already in progress.
async fn read_composition_policy(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> composition::CompositionPolicy {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<composition::CompositionPolicy>()
                .ok()
                .map(|policy| (*policy).clone())
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads the explicitly installed gate planner mode, following the same
/// resource-only discipline [`read_composition_policy`] does:
/// `main.rs` reads and validates `POLARIS_CODE_GATE_PLANNER` once at
/// startup, and no resource means [`planner::GatePlannerMode::Scripted`].
async fn read_gate_planner_mode(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> planner::GatePlannerMode {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<planner::GatePlannerMode>()
                .ok()
                .map(|mode| *mode)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads the explicitly installed domain/arm selection, following the same
/// resource-only discipline [`read_composition_policy`] does: `main.rs`
/// reads and validates `POLARIS_CODE_DOMAIN`/`POLARIS_CODE_ARM` once at
/// startup, and no resource means [`domains::DomainSelection::default`] — no
/// domain, the generic arm, which runs the unmodified `scripted_plan`.
async fn read_domain_selection(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> domains::DomainSelection {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<domains::DomainSelection>()
                .ok()
                .map(|selection| *selection)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Reads the explicitly installed gate-plan materialisation limits, following
/// the same resource-only discipline [`read_composition_policy`] does. No
/// resource means [`polaris_cosmos::composition::CompositionLimits::conservative`].
async fn read_gate_plan_limits(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> planner::GatePlanLimits {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<planner::GatePlanLimits>()
                .ok()
                .map(|limits| *limits)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// The [`ExclusionReason`] recorded when native live admission itself refuses
/// a freshly materialised gate-plan graph — the same boundary
/// [`readmit_materialized_graph`] checks for any graph built outside the
/// topology catalog.
fn gate_plan_live_admission_refusal(
    candidate: &RegistrationSelectionRef,
    error: &MaterializedGraphAdmissionError,
) -> Result<ExclusionReason, TopologySelectionError> {
    let contract_error = |error: polaris_cosmos::contracts::ContractError| {
        TopologySelectionError::Admission(error.to_string())
    };
    let subject =
        RecordSubjectId::new(candidate.registration_id().as_str()).map_err(contract_error)?;
    let code = ExclusionCode::new("composition.gate-plan-live-admission-refused")
        .map_err(contract_error)?;
    ExclusionReason::new(
        ExclusionPhase::LiveReadmission,
        code,
        vec![subject],
        Vec::new(),
    )
    .with_detail(error.to_string())
    .map_err(contract_error)
}

/// Performs live admission for a graph [`planner::attempt_gate_plan`]
/// materialised, mirroring [`attempt_live_admission`] but through
/// [`readmit_materialized_graph`] rather than the sealed-catalog boundary
/// [`readmit_live`] checks: a gate plan names no catalog registration to
/// re-read, so there is no generation or digest to compare against, only the
/// graph itself and the live context.
#[expect(
    clippy::too_many_arguments,
    reason = "live readmission joins the decision, the frozen snapshots, and the pristine context"
)]
fn attempt_gate_plan_live_admission(
    frozen: &FrozenCatalog,
    availability: &RuntimeAvailabilitySnapshot,
    context: &SystemContext<'static>,
    executor: &GraphExecutor,
    candidate: &RegistrationSelectionRef,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    sequence: LogicalSequence,
    graph: &polaris::graph::Graph,
) -> Result<(AdmissionEvaluation, Option<RegistrationSelectionRef>), TopologySelectionError> {
    let recorded_at = RecordedAt::new(sequence, Some(unix_epoch_ms()));
    let (result, permitted) =
        match readmit_materialized_graph(graph, executor, context, decision_id, attempt_id) {
            Ok(permit) => {
                let permitted = permit
                    .consume(decision_id, attempt_id, graph, context)
                    .ok()
                    .map(|()| candidate.clone());
                (AdmissionResult::Admitted, permitted)
            }
            Err(error) => {
                let reason = gate_plan_live_admission_refusal(candidate, &error)?;
                let result = AdmissionResult::excluded(vec![reason])
                    .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
                (result, None)
            }
        };
    let evaluation = AdmissionEvaluation::new(
        attempt_id,
        decision_id,
        candidate.clone(),
        frozen.snapshot.content_digest(),
        availability.snapshot_id(),
        availability.content_digest(),
        result,
        recorded_at,
    )
    .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
    Ok((evaluation, permitted))
}

/// Builds the same shape a live-admission refusal would
/// (`ExclusionPhase::LiveReadmission`, one canonical [`ExclusionReason`]) for
/// a composition-chooser call that could not turn into a request.
///
/// This is what makes a chooser failure "an ordinary refusal, not a wiring
/// fault": the resulting [`AdmissionEvaluation`] is appended exactly like a
/// live-admission attempt's, so `select_and_run`'s loop can `continue` to the
/// next fallback candidate and the caller's own accumulated `refusals` still
/// end up in the turn's `SelectionNotStarted`/`NoTopologyStarted` answer when
/// nothing starts. The digests recorded in
/// [`CompositionPolicyError::InvalidModelAnswer`] become this reason's
/// `snapshot_refs`, so a discarded candidate's billed call is still traceable
/// evidence, not a number that only shows up as a meter delta.
fn chooser_refusal_evaluation(
    candidate: &RegistrationSelectionRef,
    frozen: &FrozenCatalog,
    availability: &RuntimeAvailabilitySnapshot,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    recorded_at: RecordedAt,
    error: &composition::CompositionPolicyError,
) -> Result<AdmissionEvaluation, TopologySelectionError> {
    let snapshot_refs = match error {
        composition::CompositionPolicyError::InvalidModelAnswer {
            prompt_digest,
            response_digest,
            ..
        } => vec![*prompt_digest, *response_digest],
        composition::CompositionPolicyError::ModelUnavailable
        | composition::CompositionPolicyError::Model(_)
        | composition::CompositionPolicyError::InvalidAnswer(_) => Vec::new(),
    };
    let contract_error = |error: polaris_cosmos::contracts::ContractError| {
        TopologySelectionError::Admission(error.to_string())
    };
    let subject =
        RecordSubjectId::new(candidate.registration_id().as_str()).map_err(contract_error)?;
    let code = ExclusionCode::new("composition.chooser-refused").map_err(contract_error)?;
    let reason = ExclusionReason::new(
        ExclusionPhase::LiveReadmission,
        code,
        vec![subject],
        snapshot_refs,
    )
    .with_detail(error.to_string())
    .map_err(contract_error)?;
    let result = AdmissionResult::excluded(vec![reason]).map_err(contract_error)?;
    AdmissionEvaluation::new(
        attempt_id,
        decision_id,
        candidate.clone(),
        frozen.snapshot.content_digest(),
        availability.snapshot_id(),
        availability.content_digest(),
        result,
        recorded_at,
    )
    .map_err(contract_error)
}

/// The [`ExclusionReason`] recorded when [`composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN`]
/// is spent before `candidate` could even be tried.
///
/// No attempt id is minted for this one: unlike every other reason in
/// `refusals`, it does not correspond to a call that was actually made, so it
/// carries no admission attempt and bills nothing. It exists so the turn's
/// final answer explains *why* an otherwise-eligible candidate was never
/// tried, rather than leaving a reader to wonder whether the catalog ran out
/// of candidates or the host simply gave up.
fn chooser_budget_exhausted_reason(
    candidate: &RegistrationSelectionRef,
) -> Result<ExclusionReason, TopologySelectionError> {
    let contract_error = |error: polaris_cosmos::contracts::ContractError| {
        TopologySelectionError::Admission(error.to_string())
    };
    let subject =
        RecordSubjectId::new(candidate.registration_id().as_str()).map_err(contract_error)?;
    let code =
        ExclusionCode::new("composition.chooser-budget-exhausted").map_err(contract_error)?;
    ExclusionReason::new(
        ExclusionPhase::LiveReadmission,
        code,
        vec![subject],
        Vec::new(),
    )
    .with_detail(format!(
        "the model composition policy already made {} real chooser call(s) this turn, its \
             per-turn ceiling; remaining fallback candidates were not tried",
        composition::MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN
    ))
    .map_err(contract_error)
}

/// Reads the sealed catalog snapshot, the coding contract, and every known
/// configuration schema in one borrow of the global catalog.
async fn read_catalog(
    sessions: &SessionsAPI,
    session_id: &SessionId,
) -> Result<FrozenCatalog, TopologySelectionError> {
    let read = sessions
        .with_context(session_id, |ctx| {
            let catalog = ctx.get_resource::<TopologyCatalog>().ok()?;
            Some(catalog.snapshot().map_err(|error| error.to_string()))
        })
        .await
        .ok()
        .flatten()
        .ok_or(TopologySelectionError::CapabilityMissing(
            "TopologyCatalog capability",
        ))?;
    let snapshot = read.map_err(TopologySelectionError::Catalog)?;
    let contract = snapshot
        .contracts()
        .iter()
        .find(|contract| contract.key().id().as_str() == topology::CONTRACT)
        .cloned()
        .ok_or_else(|| {
            TopologySelectionError::Catalog(format!(
                "the sealed catalog holds no '{}' contract",
                topology::CONTRACT
            ))
        })?;
    let known_schemas = snapshot
        .candidates()
        .iter()
        .filter_map(|candidate| candidate.admission_metadata())
        .map(|metadata| metadata.configuration_schema().clone())
        .collect();
    Ok(FrozenCatalog {
        snapshot,
        contract,
        known_schemas,
    })
}

/// Readmits one captured candidate against the live catalog.
///
/// The catalog is borrowed for exactly this call — the permit's
/// context-identity check binds it to `context`, so both the readmission and
/// the permit consumption happen before the borrow is released and before any
/// `await`.
#[expect(
    clippy::too_many_arguments,
    reason = "live readmission joins the decision, the frozen snapshots, and the pristine context"
)]
async fn attempt_live_admission(
    sessions: &SessionsAPI,
    session_id: &SessionId,
    frozen: &FrozenCatalog,
    availability: &RuntimeAvailabilitySnapshot,
    context: &SystemContext<'static>,
    executor: &GraphExecutor,
    candidate: &RegistrationSelectionRef,
    decision_id: SelectionDecisionId,
    attempt_id: AdmissionAttemptId,
    sequence: LogicalSequence,
) -> Result<
    (
        polaris_cosmos::contracts::AdmissionEvaluation,
        Option<RegistrationSelectionRef>,
    ),
    TopologySelectionError,
> {
    let native = frozen
        .snapshot
        .candidates()
        .iter()
        .find(|entry| entry.registration_id() == candidate.registration_id())
        .ok_or_else(|| {
            TopologySelectionError::Admission(format!(
                "selected candidate '{}' is not in the catalog snapshot",
                candidate.registration_id().as_str()
            ))
        })?;
    let metadata = native.admission_metadata().ok_or_else(|| {
        TopologySelectionError::Admission(format!(
            "candidate '{}' carries no admission metadata",
            candidate.registration_id().as_str()
        ))
    })?;
    let live_candidate = AdmissionCandidate::new(
        native.clone(),
        metadata.selection().clone(),
        metadata.requirements().clone(),
    );
    let recorded_at = RecordedAt::new(sequence, Some(unix_epoch_ms()));

    let attempt = sessions
        .with_context(session_id, |ctx| {
            let catalog = ctx.get_resource::<TopologyCatalog>().ok()?;
            // Re-read the live digest rather than reusing the frozen one, so a
            // catalog that changed under the decision is refused as stale
            // instead of silently accepted.
            let current = catalog.snapshot().ok()?.content_digest();
            let live = readmit_live(
                &catalog,
                frozen.snapshot.generation(),
                frozen.snapshot.content_digest(),
                current,
                &live_candidate,
                &frozen.contract,
                availability,
                &frozen.known_schemas,
                catalog.type_explanations(),
                executor,
                context,
                decision_id,
                attempt_id,
                recorded_at,
            );
            Some(live.map(|live| {
                let (evaluation, permit) = live.into_parts();
                let permitted = permit.and_then(|permit| {
                    permit
                        .consume(decision_id, evaluation.candidate(), context)
                        .ok()
                });
                (evaluation, permitted)
            }))
        })
        .await
        .ok()
        .flatten()
        .ok_or(TopologySelectionError::CapabilityMissing(
            "TopologyCatalog capability",
        ))?;
    attempt.map_err(|error| TopologySelectionError::Admission(error.to_string()))
}

/// Opens the run, executes the permitted native graph, finalizes it, and
/// appends this host's real outcome observations.
///
/// `local_composition_tail`, when given, supplies the `CompositionRun` and
/// `CompositionOutcome` records' own `recorded_at` values instead of the real
/// `RunOpened`/outcome-observation positions this function otherwise reuses.
/// A gate-plan turn's earlier revisions (refused or admitted) are numbered
/// from a small counter kept apart from the store — nothing real to anchor a
/// refused attempt to exists, since materialisation refuses before a graph
/// does — so the run and outcome that close out the same trail have to
/// continue that same local counter too: reusing the real store's positions
/// here would reintroduce exactly the ordering hazard the local counter
/// exists to avoid, since the real store can still be at a low position (a
/// fresh turn's own admission can land as early as position two) regardless
/// of how many local-only revisions preceded it. `None` keeps the ordinary
/// finite-recipe behaviour, reusing the real positions as before.
#[expect(
    clippy::too_many_arguments,
    reason = "the run lifecycle joins the decision, admission, evidence, and pristine context"
)]
async fn execute_permitted(
    evidence: &TopologyEvidence,
    contract_key: &polaris_cosmos::topology::TopologyContractKey,
    ids: &TurnEventIds,
    decision: &SelectionDecision,
    admission_event: &polaris_cosmos::contracts::AdmissionEvaluation,
    admitted_validation_recorded_at: RecordedAt,
    permitted: &RegistrationSelectionRef,
    composition: composition::MaterializedComposition,
    fallback_position: u64,
    rationale: SelectionRationale,
    executor: &GraphExecutor,
    context: &mut SystemContext<'static>,
    workspace: &CodingWorkspace,
    accounting: &RunAccounting,
    host: HostCapabilities,
    task: &CodingTask,
    middleware: Option<&MiddlewareAPI>,
    requested: Option<String>,
    requested_by_pin: bool,
    local_composition_tail: Option<(RecordedAt, RecordedAt)>,
) -> Result<ExecutedTopology, TopologySelectionError> {
    let registration = permitted.registration_id().clone();
    let composition_request = composition.request().clone();
    if !composition
        .graph()
        .signature()
        .satisfies(&GraphSignature::new().produce::<AssessmentConclusion>())
    {
        return Err(TopologySelectionError::Admission(format!(
            "'{}' does not satisfy the coding contract",
            registration.as_str()
        )));
    }

    let cosmos_run_id = ids.run()?;
    // The caller already worked out where this validation belongs: a
    // finite-recipe decision reuses the turn's own real admission position
    // (always safely behind it), while a gate-plan decision's own revision
    // loop reserved a local sequence for exactly this validation, strictly
    // below the real admission position it never reused. Either way it is
    // supplied here rather than re-derived, so there is exactly one place
    // that decides it.
    let composition_validation = composition
        .admitted_validation(
            admission_event.attempt_id(),
            admitted_validation_recorded_at,
        )
        .map_err(|error| TopologySelectionError::Admission(error.to_string()))?;
    let opened = RunOpened::new(
        cosmos_run_id,
        RecordedAt::new(next_sequence(evidence)?, Some(unix_epoch_ms())),
        decision,
        admission_event,
        vec![contract_key.clone()],
        fallback_position,
        execution_provenance(host)?,
    )?;
    append(evidence, EvidenceEvent::RunOpened(opened.clone()))?;
    let (run_recorded_at, outcome_recorded_at_override) = match local_composition_tail {
        Some((run_recorded_at, outcome_recorded_at)) => {
            (run_recorded_at, Some(outcome_recorded_at))
        }
        None => (opened.recorded_at(), None),
    };
    let composition_run = polaris_cosmos::contracts::CompositionRun::new(
        cosmos_run_id,
        &composition_validation,
        run_recorded_at,
    )?;

    let started = Instant::now();
    // The same middleware chain the session's own turn runs under, so a node
    // of this graph is as watchable as a node of the agent loop. `None` is an
    // empty chain: identical execution, nobody informed.
    let execution = match executor
        .execute(composition.graph(), context, None, middleware)
        .await
    {
        Ok(execution) => execution,
        Err(error) if accounting.meter.cost_cap_stopped() => {
            let elapsed = started.elapsed();
            let usage = accounting.snapshot();
            let finalized = RunFinalized::new(
                opened.run_id(),
                RecordedAt::new(next_sequence(evidence)?, Some(unix_epoch_ms())),
                DurationMs::new(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
                RunStructure::new(0, 0, Some(0), 1, 1, Vec::new())?,
                RunControlFlow::empty(),
                RunFaults::new(
                    vec![ExecutionFault::new(
                        ErrorClassId::new("cost-cap")?,
                        ExecutionStage::Execution,
                        None,
                    )?],
                    0,
                    0,
                    None,
                ),
                cosmos_usage(usage.as_ref(), host)?,
                RunCompletion::new(
                    ExecutionDisposition::Failed,
                    Some(StopReason::BudgetExhausted),
                    None,
                    None,
                )?,
                Vec::new(),
            )?;
            append(evidence, EvidenceEvent::RunFinalized(finalized))?;
            return Err(TopologySelectionError::Execution(format!(
                "cost-cap: {error}"
            )));
        }
        Err(error) => {
            return Err(TopologySelectionError::Execution(format!(
                "coding graph execution failed: {error}"
            )));
        }
    };
    let elapsed = started.elapsed();
    // The contract's declared output is the whole of what the host reads back.
    // A registration that produced none, or one that accounted for nothing,
    // fails here and visibly — never as a turn that answers with an empty
    // string.
    let conclusion = execution.output::<AssessmentConclusion>().ok_or_else(|| {
        TopologySelectionError::Execution(
            "the selected coding topology produced no conclusion".into(),
        )
    })?;
    // A graph's conclusion alone does not establish that a statement-only
    // edit happened. Read graph-owned provenance rather than the filesystem:
    // test reports, caches, and build products are not edits.
    let applied_edit = execution
        .output::<AppliedEdit>()
        .is_some_and(|edit| edit.outcomes.iter().any(|outcome| outcome.wrote()));
    let product = TopologyProduct::for_task(conclusion, task, applied_edit);
    if product.answer().is_none() {
        return Err(TopologySelectionError::Execution(format!(
            "'{}' concluded without assessing anything, so it has no answer to return",
            registration.as_str()
        )));
    }

    // The run's own meter, read once the graph has returned. Whatever a
    // model-calling node billed is here, broken out under this run's own
    // model id; it is also folded onto the session's total once this call
    // returns (see `SessionMeterMerge`).
    let run_usage = accounting.snapshot();

    // Finalization is mechanical: it records that the graph ran, and makes no
    // claim about whether the coding task succeeded.
    let nodes = u64::try_from(execution.nodes_executed()).unwrap_or(u64::MAX);
    let finalized = RunFinalized::new(
        opened.run_id(),
        RecordedAt::new(next_sequence(evidence)?, Some(unix_epoch_ms())),
        DurationMs::new(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
        RunStructure::new(nodes, nodes, Some(0), 1, 1, Vec::new())?,
        RunControlFlow::empty(),
        RunFaults::empty(),
        cosmos_usage(run_usage.as_ref(), host)?,
        RunCompletion::new(
            ExecutionDisposition::Completed,
            Some(StopReason::PredicateSatisfied),
            Some(CoveragePpm::ONE),
            Some(0),
        )?,
        Vec::new(),
    )?;
    append(evidence, EvidenceEvent::RunFinalized(finalized))?;

    // Judgement is the embodiment's: the stated edits are checked here, and
    // whether the *work* succeeded is the caller's own check, run second.
    let task_id = decision.task_id().clone();
    let verified = workspace
        .verify()
        .map_err(|error| TopologySelectionError::Execution(error.to_string()))?;
    let applied_id = ids.applied_observation()?;
    let applied = verify::applied_edit_observation(
        workspace,
        &verified,
        applied_id,
        opened.run_id(),
        task_id.clone(),
        next_sequence(evidence)?.get(),
    )?;
    append(evidence, EvidenceEvent::Observation(applied.clone()))?;
    let outcome = verify::work_outcome_observation(
        task.success(),
        workspace,
        &verify::ObservationSlot {
            observation_id: ids.outcome_observation()?,
            run_id: opened.run_id(),
            task_id: task_id.clone(),
            sequence: next_sequence(evidence)?.get(),
            delay_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
            supersedes: applied_id,
        },
    )?;
    append(evidence, EvidenceEvent::Observation(outcome.clone()))?;
    let composition_outcome = polaris_cosmos::contracts::CompositionOutcome::new(
        outcome.observation_id(),
        &composition_run,
        outcome_recorded_at_override.unwrap_or_else(|| outcome.recorded_at()),
    )?;
    let composition_evidence = polaris_cosmos::contracts::CompositionEvidence::new(
        composition.decision().clone(),
        composition_validation,
        composition_run,
        composition_outcome,
    )?;

    Ok(ExecutedTopology {
        decision_id: decision.decision_id(),
        task_id,
        registration_id: registration.as_str().to_owned(),
        fallback_position,
        cosmos_run_id,
        run_id: execution.run_id().clone(),
        nodes_executed: execution.nodes_executed(),
        duration: elapsed,
        product,
        usage: run_usage,
        observations: vec![applied, outcome],
        composition: Some(composition_evidence),
        composition_request: Some(composition_request),
        // Overwritten by the caller when this run's composition came from a
        // gate plan; `execute_permitted` builds only the ordinary
        // finite-recipe-shaped `CompositionEvidence` above, regardless of
        // which composition policy produced it.
        gate_plan_trail: None,
        gate_plan_shape: None,
        // Overwritten by the caller when this candidate's own gate-plan
        // revision loop exhausted before falling back to the finite recipe
        // grammar `execute_permitted` actually ran.
        exhausted_gate_plan_trail: None,
        requested,
        requested_by_pin,
        // Overwritten by the caller with the accepted task's real origin;
        // `execute_permitted` is not handed the accepted task itself.
        origin: TaskOrigin::Explicit,
        candidates: decision.candidates().to_vec(),
        rationale,
    })
}

/// Appends one event, converting a provider rejection into a typed error.
fn append(evidence: &TopologyEvidence, event: EvidenceEvent) -> Result<(), TopologySelectionError> {
    evidence
        .append(event)
        .map(|_| ())
        .map_err(|error| TopologySelectionError::Evidence(error.detail().to_owned()))
}

/// Returns the sequence the store will accept next.
///
/// Read from the provider rather than counted here, so the host never assumes
/// it is the only appender and a bounded store's own accounting stays
/// authoritative.
fn next_sequence(evidence: &TopologyEvidence) -> Result<LogicalSequence, TopologySelectionError> {
    let status = evidence
        .store_status()
        .map_err(|error| TopologySelectionError::Evidence(error.detail().to_owned()))?;
    Ok(LogicalSequence::new(
        status.observed_through.get().saturating_add(1),
    ))
}

/// Milliseconds since the Unix epoch, saturating rather than panicking on a
/// clock before it.
fn unix_epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_default()
}

/// Attribution for one live selected run.
///
/// The provider is claimed only when the accepted task said the run context
/// would carry one — the same declaration `probe_availability` reports the
/// registry under. No [`ModelProvenance`](polaris_cosmos::contracts::ModelProvenance)
/// entry is written: which model a graph drove is the graph's choice, made
/// after this record is sealed, so naming one here would be a guess.
fn execution_provenance(
    host: HostCapabilities,
) -> Result<ExecutionProvenance, polaris_cosmos::contracts::ContractError> {
    let providers = if host.model_provider {
        vec![topology::model_provider_key()]
    } else {
        Vec::new()
    };
    ExecutionProvenance::new(
        EmbodimentKey::new(
            EmbodimentId::new("polaris-code.coding-embodiment")?,
            super::behavior_version(),
        ),
        HostRuntimeKey::new(
            HostRuntimeId::new("polaris-code.terminal-host")?,
            super::behavior_version(),
        ),
        providers,
        Vec::new(),
        vec![provenance(
            SourceKind::HostObservation,
            EpistemicBasis::Verified,
        )],
    )
}

/// Stable identity of the token accounting scheme [`TokenMeter`] counts under.
///
/// One scheme for the whole meter: cache-read and cache-creation tokens are
/// folded into the input count, because Cosmos [`TokenUsage`] carries one input
/// and one output quantity per (provider, model, scheme) and splitting the
/// tiers would need three schemes to stay comparable. The tier detail is not
/// lost — [`ExecutedTopology::usage`] reports it in full.
const TOKEN_ACCOUNTING: &str = "polaris-code.billed-tokens";

/// Projects the run's own meter into the mechanical usage Cosmos records.
///
/// Empty when the run spent nothing, when the run was unmeasured, or when the
/// host never declared a provider — an entry needs a provider key, and
/// inventing one would be an unearned provenance claim.
fn cosmos_usage(
    usage: Option<&crate::usage::RunUsage>,
    host: HostCapabilities,
) -> Result<RunUsage, polaris_cosmos::contracts::ContractError> {
    let Some(usage) = usage.filter(|usage| host.model_provider && usage.total_tokens > 0) else {
        return Ok(RunUsage::empty());
    };
    let Ok(model) = ModelId::new(usage.model_id.as_str()) else {
        // A model id the host configured but Cosmos cannot name is a
        // configuration problem, not a reason to fail a run that has already
        // finished. The full accounting is still on `ExecutedTopology::usage`.
        tracing::warn!(
            model_id = usage.model_id,
            "model id is not a Cosmos stable name; the run's tokens are reported \
             on the executed topology but omitted from its evidence record"
        );
        return Ok(RunUsage::empty());
    };
    let accounting = TokenAccountingId::new(TOKEN_ACCOUNTING)?;
    RunUsage::new(
        vec![TokenUsage::new(
            topology::model_provider_key(),
            model,
            TokenQuantity::new(
                accounting.clone(),
                usage.input_tokens + usage.cache_read_tokens + usage.cache_creation_tokens,
            ),
            TokenQuantity::new(accounting, usage.output_tokens),
        )?],
        None,
        None,
    )
}

/// The evidence event identities one turn allocates.
///
/// Cosmos requires caller-allocated `UUIDv4` event identities and this crate
/// has no UUID dependency, so each one is derived from
/// `(session id, turn number, event label)` by SHA-256 with the version and
/// variant bits set. That keeps them collision-resistant, keyed to the run
/// identity, and replayable: the same turn always writes the same episode.
struct TurnEventIds {
    seed: String,
}

impl TurnEventIds {
    fn new(session_id: &SessionId, turn_number: TurnNumber) -> Self {
        Self {
            seed: format!("{}#{turn_number}", session_id.as_str()),
        }
    }

    /// Derives a `UUIDv4`-shaped identity for one labelled event of this turn.
    fn bytes(&self, label: &str) -> [u8; 16] {
        let digest = ContentDigest::sha256(format!("{}#{label}", self.seed).as_bytes());
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&digest.as_bytes()[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        bytes
    }

    fn availability(&self) -> Result<AvailabilitySnapshotId, TopologySelectionError> {
        Ok(AvailabilitySnapshotId::from_bytes(
            self.bytes("availability"),
        )?)
    }

    fn decision(&self) -> Result<SelectionDecisionId, TopologySelectionError> {
        Ok(SelectionDecisionId::from_bytes(self.bytes("decision"))?)
    }

    fn admission(&self, position: usize) -> Result<AdmissionAttemptId, TopologySelectionError> {
        Ok(AdmissionAttemptId::from_bytes(
            self.bytes(&format!("admission.{position}")),
        )?)
    }

    /// Identifies one refused gate-plan revision attempt, distinct from the
    /// real live-admission attempt its candidate position names — a refused
    /// attempt never reaches live admission, so it needs an identity of its
    /// own rather than borrowing one that was never actually used.
    fn gate_plan_attempt(
        &self,
        position: usize,
        attempt: RevisionAttempt,
    ) -> Result<AdmissionAttemptId, TopologySelectionError> {
        Ok(AdmissionAttemptId::from_bytes(self.bytes(&format!(
            "admission.{position}.gate-plan.{}",
            attempt.get()
        )))?)
    }

    fn run(&self) -> Result<RunId, TopologySelectionError> {
        Ok(RunId::from_bytes(self.bytes("run"))?)
    }

    fn applied_observation(&self) -> Result<OutcomeObservationId, TopologySelectionError> {
        Ok(OutcomeObservationId::from_bytes(
            self.bytes("observation.applied"),
        )?)
    }

    fn outcome_observation(&self) -> Result<OutcomeObservationId, TopologySelectionError> {
        Ok(OutcomeObservationId::from_bytes(
            self.bytes("observation.outcome"),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(dir: &std::path::Path) -> CodingTask {
        CodingTask::new(
            "queue-probe",
            dir,
            "src/lib.rs",
            "todo!()",
            "0",
            "todo!()",
            super::super::task::CodingIntent::Careful,
        )
        .expect("the coding task is well formed")
    }

    #[test]
    fn the_queue_yields_one_accepted_task_per_take_in_order() {
        let dir = tempfile::tempdir().expect("temp dir");
        let queue = CodingTaskQueue::new();
        assert!(
            queue.is_empty(),
            "a fresh queue leaves turns conversational"
        );

        queue.accept(AcceptedCodingTask::new(task(dir.path())));
        queue.accept(
            AcceptedCodingTask::new(task(dir.path()))
                .with_objective(CodingObjective::new(super::super::task::CodingIntent::Fast)),
        );
        assert_eq!(queue.len(), 2);

        let first = queue.take().expect("the first accepted task");
        assert_eq!(queue.len(), 1, "exactly one task leaves the queue per take");
        assert!(matches!(
            first.task().intent(),
            super::super::task::CodingIntent::Careful
        ));
        let second = queue.take().expect("the second accepted task");
        assert!(queue.is_empty());
        assert!(second.task().working_dir().is_dir());
        assert!(
            queue.take().is_none(),
            "a drained queue stays conversational"
        );
    }

    #[test]
    fn a_clone_shares_the_same_queue() {
        let dir = tempfile::tempdir().expect("temp dir");
        let host = CodingTaskQueue::new();
        let published = host.clone();
        host.accept(AcceptedCodingTask::new(task(dir.path())));
        assert_eq!(
            published.len(),
            1,
            "the plugin's published handle sees what the host accepted"
        );
        assert!(published.take().is_some());
        assert!(
            host.is_empty(),
            "the run surface's take drains the host's own queue"
        );
    }

    #[test]
    fn turn_event_ids_are_uuid_v4_distinct_and_replayable() {
        let session = SessionId::from_string("session-one");
        let ids = TurnEventIds::new(&session, 7);
        let replay = TurnEventIds::new(&session, 7);

        // Every derived identity passes Cosmos's own UUIDv4 validation.
        let decision = ids.decision().expect("a decision ID is a UUIDv4");
        let run = ids.run().expect("a run ID is a UUIDv4");
        let availability = ids.availability().expect("an availability ID is a UUIDv4");
        ids.applied_observation().expect("applied is a UUIDv4");
        ids.outcome_observation().expect("outcome is a UUIDv4");

        // Replaying the same turn writes the same episode.
        assert_eq!(decision, replay.decision().expect("replayed decision"));
        assert_eq!(run, replay.run().expect("replayed run"));

        // Distinct labels, turns, and sessions never collide.
        assert_ne!(decision.to_string(), run.to_string());
        assert_ne!(decision.to_string(), availability.to_string());
        assert_ne!(
            ids.admission(0).expect("primary attempt"),
            ids.admission(1).expect("first fallback attempt")
        );
        let next_turn = TurnEventIds::new(&session, 8);
        assert_ne!(decision, next_turn.decision().expect("next turn decision"));
        let other_session = TurnEventIds::new(&SessionId::from_string("session-two"), 7);
        assert_ne!(decision, other_session.decision().expect("other session"));
    }

    #[test]
    fn auto_route_recognises_answerable_questions_and_ignores_the_rest() {
        let dir = tempfile::tempdir().expect("temp dir");

        let recognised = auto_route(dir.path(), "where is fn add")
            .expect("a lexically recognised question is routed");
        assert_eq!(recognised.origin(), TaskOrigin::AutoRouted);
        assert!(matches!(recognised.task().intent(), CodingIntent::Answer));
        assert_eq!(
            recognised.task().question().asked(),
            "where does `fn add` occur"
        );

        assert!(
            auto_route(dir.path(), "please refactor this module for me").is_none(),
            "a phrasing outside the recognised shapes is not routed, so an ordinary \
             conversational turn is never even profiled"
        );
        assert!(
            auto_route(std::path::Path::new("/does/not/exist"), "where is fn add").is_none(),
            "a working directory that does not exist cannot be routed either"
        );
    }

    #[test]
    fn auto_route_is_deterministic() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = auto_route(dir.path(), "what is in src/lib.rs").expect("a recognised question");
        let second =
            auto_route(dir.path(), "what is in src/lib.rs").expect("the same question again");
        assert_eq!(
            first.task().question().asked(),
            second.task().question().asked()
        );
        assert_eq!(first.task().statement(), second.task().statement());
        assert_eq!(first.origin(), second.origin());
    }

    #[test]
    fn is_conclusive_reads_the_product_kind_and_no_start_is_never_conclusive() {
        let session = SessionId::from_string("conclusive-probe");
        let ids = TurnEventIds::new(&session, 0);
        let decision_id = ids.decision().expect("decision id");
        let cosmos_run_id = ids.run().expect("run id");
        let task_id =
            TaskId::new(super::super::TASK_NAMESPACE, "conclusive-probe").expect("valid task id");
        let rationale = SelectionRationale::new(
            vec![
                polaris_cosmos::contracts::ScoreComponentId::new("test.only-candidate")
                    .expect("valid score component id"),
            ],
            false,
            false,
            None,
            policy("test-confidence-policy"),
        )
        .expect("valid rationale");

        let executed = |kind: ConclusionKind, origin: TaskOrigin| {
            TopologyTurn::Executed(Box::new(ExecutedTopology {
                decision_id,
                task_id: task_id.clone(),
                registration_id: "code.tree-question".to_owned(),
                fallback_position: 0,
                cosmos_run_id,
                run_id: polaris::graph::RunId::new(),
                nodes_executed: 3,
                duration: Duration::ZERO,
                product: TopologyProduct {
                    kind,
                    stop_reason: StopReason::GoalMet,
                    pass: 1,
                    issue: 0,
                    not_applicable: 0,
                    indeterminate: 0,
                },
                usage: None,
                observations: Vec::new(),
                composition: None,
                composition_request: None,
                gate_plan_trail: None,
                gate_plan_shape: None,
                exhausted_gate_plan_trail: None,
                requested: None,
                requested_by_pin: false,
                origin,
                candidates: Vec::new(),
                rationale: rationale.clone(),
            }))
        };

        assert!(executed(ConclusionKind::Resolved, TaskOrigin::AutoRouted).is_conclusive());
        assert!(executed(ConclusionKind::Unresolved, TaskOrigin::AutoRouted).is_conclusive());
        assert!(
            !executed(ConclusionKind::Escalated, TaskOrigin::AutoRouted).is_conclusive(),
            "an escalated answer is the one conclusion an auto-routed accept treats as unusable"
        );
        // `is_conclusive` reads the product alone; the caller, not this
        // method, is what makes `TaskOrigin::Explicit` ignore it.
        assert!(!executed(ConclusionKind::Escalated, TaskOrigin::Explicit).is_conclusive());
        assert_eq!(
            executed(ConclusionKind::Resolved, TaskOrigin::AutoRouted).origin(),
            TaskOrigin::AutoRouted
        );

        let no_eligible = TopologyTurn::NoEligibleCandidate(Box::new(NoTopologyStarted {
            decision_id,
            task_id: task_id.clone(),
            reasons: vec!["capability.forbidden".to_owned()],
            attempted: Vec::new(),
            requested: None,
            requested_by_pin: false,
            origin: TaskOrigin::AutoRouted,
        }));
        assert!(!no_eligible.is_conclusive());
        assert_eq!(no_eligible.origin(), TaskOrigin::AutoRouted);

        let not_started = TopologyTurn::NotStarted(Box::new(NoTopologyStarted {
            decision_id,
            task_id,
            reasons: vec!["admission.refused".to_owned()],
            attempted: vec!["code.tree-question".to_owned()],
            requested: None,
            requested_by_pin: false,
            origin: TaskOrigin::Explicit,
        }));
        assert!(!not_started.is_conclusive());
        assert_eq!(not_started.origin(), TaskOrigin::Explicit);
    }

    fn resources_with_shell(
        executor: polaris::shell::ShellExecutor,
    ) -> polaris::system::resource::Resources {
        let mut resources = polaris::system::resource::Resources::new();
        resources.insert(executor);
        resources
    }

    fn permissive_shell() -> polaris::shell::ShellExecutor {
        polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_allowed_commands(vec!["python *".to_owned()])
                .disable_sandbox(),
        )
    }

    #[test]
    fn reproduce_runner_available_requires_an_actual_allow_verdict() {
        // No `ShellExecutor` global at all: unlike `effective_host_capabilities`,
        // which trusts an explicit caller-stated command when nothing governs
        // it, nobody asked for this command — a hermetic host must read as no
        // test runner, not as "nothing to check against, so assume yes."
        assert!(!reproduce_runner_available(None));

        // A `ShellExecutor` present, but its allow-list matches something
        // else entirely: still no runner.
        let narrow = resources_with_shell(polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_allowed_commands(vec!["cargo *".to_owned()])
                .disable_sandbox(),
        ));
        assert!(!reproduce_runner_available(Some(&narrow)));

        // An allow-list matching the reproduce block's own program: available.
        let permissive = resources_with_shell(permissive_shell());
        assert!(reproduce_runner_available(Some(&permissive)));
    }

    #[test]
    fn reproduce_runner_available_is_narrower_than_effective_host_capabilities() {
        // The point of keeping this a separate fact from `HostCapabilities`:
        // a *hermetic* declared capability (`test_runner: false`) for a
        // statement-only (`Delegated`) task leaves `effective_host_capabilities`
        // at `false` even under a permissive shell policy, since that function
        // only ever narrows an explicit `Command` check, never widens a
        // `Delegated` one — while `reproduce_runner_available` reads the very
        // same policy directly and reports it available.
        let permissive = resources_with_shell(permissive_shell());
        let task = CodingTask::from_statement(
            "host-fact-fixture",
            std::env::temp_dir(),
            "add() subtracts instead of adding",
            super::super::task::CodingIntent::Careful,
            SuccessCheck::delegated("external-verifier").expect("a fixed verifier name is valid"),
        )
        .expect("well-formed statement-only task");
        let declared = HostCapabilities::hermetic();

        assert!(
            !effective_host_capabilities(&task, declared, Some(&permissive)).test_runner,
            "a Delegated task's effective test runner stays exactly what the host declared"
        );
        assert!(reproduce_runner_available(Some(&permissive)));
    }
}
