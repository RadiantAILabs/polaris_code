//! Approved reusable cognition blocks and replaceable model points.
//!
//! The items in this module assemble ordinary Polaris graphs. They do not add
//! an executor, a graph abstraction, or a universal model trait. Embodiments
//! supply typed systems and policies; Cosmos fixes only the approved ordering,
//! routing, accounting commitments, roles, and discoverable point metadata.

use std::collections::VecDeque;
use std::fmt;
use std::marker::PhantomData;

use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::{SystemAccess, SystemContext};
use polaris::system::plugin::{Schedule, Version};
use polaris::system::resource::{LocalResource, Output};
use polaris::system::system::{BoxFuture, BoxedSystem, System, SystemError, TypedSystem};
use serde::Serialize;

use super::OnVerify;
use crate::contracts::{
    Assessment, AssessmentConclusion, AssessmentSummary, ContextTypeId, ContextTypeKey,
    EvidenceRef, Status, StopReason,
};
use crate::topology::{
    AdmissionBuildError, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
    TypeExplanationRegistry,
};

/// Role marker for a system that establishes the bounded subject set.
#[derive(Debug, Clone, Copy)]
pub struct OnScope;
impl Schedule for OnScope {}

/// Role marker for a system that observes one scoped subject.
#[derive(Debug, Clone, Copy)]
pub struct OnProbe;
impl Schedule for OnProbe {}

/// Role marker for a system that judges one observed subject.
#[derive(Debug, Clone, Copy)]
pub struct OnAdjudicate;
impl Schedule for OnAdjudicate {}

/// Role marker for a system that combines a complete assessment ledger.
#[derive(Debug, Clone, Copy)]
pub struct OnCombine;
impl Schedule for OnCombine {}

/// Role marker for a system that accounts for every terminal residual.
#[derive(Debug, Clone, Copy)]
pub struct OnResolve;
impl Schedule for OnResolve {}

/// Role marker for the resolver that emits an escalated conclusion.
///
/// This marker does not represent or perform an escalation mechanism. The host
/// remains responsible for acting on the emitted conclusion.
#[derive(Debug, Clone, Copy)]
pub struct OnEscalate;
impl Schedule for OnEscalate {}

/// A builder for one native branch or loop body.
pub type GraphBuilder = Box<dyn FnOnce(&mut Graph) + 'static>;

/// A boxed predicate over a typed system output.
pub type OutputPredicate<T> = Box<dyn Fn(&T) -> bool + Send + Sync + 'static>;

/// Error returned when an approved cognition contract is incomplete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CognitionError(String);

impl CognitionError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CognitionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CognitionError {}

/// Registration-time proof that an oracle was falsified, or a visible waiver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OracleQualification {
    /// Evidence that the oracle rejected an embodiment-owned known-bad input.
    RejectedNegativeControl(EvidenceRef),
    /// A versioned, reviewable exception recorded with the registration.
    Waiver(OracleWaiver),
}

impl OracleQualification {
    /// Records evidence from a known-negative control that the oracle rejected.
    #[must_use]
    pub const fn rejected_negative_control(evidence: EvidenceRef) -> Self {
        Self::RejectedNegativeControl(evidence)
    }

    fn selector_label(&self) -> String {
        match self {
            Self::RejectedNegativeControl(evidence) => format!(
                "negative-control={}/{:?}/{}",
                evidence.store(),
                evidence.kind(),
                evidence.id()
            ),
            Self::Waiver(waiver) => format!(
                "negative-control-waiver={}@{}:{}",
                waiver.id, waiver.version, waiver.reason
            ),
        }
    }
}

/// Explicit versioned exception to the negative-control requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OracleWaiver {
    id: String,
    version: Version,
    reason: String,
}

impl OracleWaiver {
    /// Creates a waiver whose identity and reason can be surfaced to selection.
    pub fn new(
        id: impl Into<String>,
        version: Version,
        reason: impl Into<String>,
    ) -> Result<Self, CognitionError> {
        let id = id.into();
        let reason = reason.into();
        if id.is_empty()
            || id.len() > 128
            || !id.chars().next().is_some_and(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit()
            })
            || !id.chars().next_back().is_some_and(|character| {
                character.is_ascii_lowercase() || character.is_ascii_digit()
            })
            || !id.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '.' | '-' | '_' | '/')
            })
        {
            return Err(CognitionError::new(
                "oracle waiver ID must be a non-empty stable lowercase identifier",
            ));
        }
        if reason.trim().is_empty() || reason.len() > 512 {
            return Err(CognitionError::new(
                "oracle waiver reason must contain 1 to 512 bytes",
            ));
        }
        Ok(Self {
            id,
            version,
            reason,
        })
    }

    /// Returns the stable waiver identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the waiver contract version.
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// Returns the reviewable reason for the exception.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Static selector-facing metadata for one replaceable cognition point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointMetadata {
    intended_fit: &'static [&'static str],
    anti_fit: &'static [&'static str],
    roles: &'static [&'static str],
    stop_reasons: &'static [StopReason],
    execution_characteristics: &'static [&'static str],
    runtime_requirements: &'static [&'static str],
    configuration_schema: &'static [&'static str],
    author_priors: &'static [&'static str],
    documentation: &'static [&'static str],
    replacement_axis: &'static str,
    alternatives: &'static [&'static str],
    oracle: OracleQualification,
}

impl PointMetadata {
    /// Returns the conditions for which the author expects the point to fit.
    #[must_use]
    pub const fn intended_fit(&self) -> &[&str] {
        self.intended_fit
    }

    /// Returns conditions that argue against selecting the point.
    #[must_use]
    pub const fn anti_fit(&self) -> &[&str] {
        self.anti_fit
    }

    /// Returns the domain-neutral role schedule names emitted by the point.
    #[must_use]
    pub const fn roles(&self) -> &[&str] {
        self.roles
    }

    /// Returns every stop reason the point may report.
    #[must_use]
    pub const fn stop_reasons(&self) -> &[StopReason] {
        self.stop_reasons
    }

    /// Returns execution-cost and side-effect characteristics.
    #[must_use]
    pub const fn execution_characteristics(&self) -> &[&str] {
        self.execution_characteristics
    }

    /// Returns native runtime mechanisms required by this point.
    #[must_use]
    pub const fn runtime_requirements(&self) -> &[&str] {
        self.runtime_requirements
    }

    /// Returns the registration configuration field descriptions.
    #[must_use]
    pub const fn configuration_schema(&self) -> &[&str] {
        self.configuration_schema
    }

    /// Returns author priors, kept separate from measured evidence.
    #[must_use]
    pub const fn author_priors(&self) -> &[&str] {
        self.author_priors
    }

    /// Returns stable documentation and behavioural-evidence references.
    #[must_use]
    pub const fn documentation(&self) -> &[&str] {
        self.documentation
    }

    /// Returns the open design axis on which this point is one option.
    #[must_use]
    pub const fn replacement_axis(&self) -> &str {
        self.replacement_axis
    }

    /// Returns concrete alternatives that can replace this point.
    #[must_use]
    pub const fn alternatives(&self) -> &[&str] {
        self.alternatives
    }

    /// Returns the required oracle qualification retained with registration.
    #[must_use]
    pub const fn oracle(&self) -> &OracleQualification {
        &self.oracle
    }

    fn selector_summary(&self, shape: &str) -> String {
        format!(
            "shape={shape}; intended-fit={}; anti-fit={}; roles={}; stops={:?}; \
             execution={}; mechanisms={}; config={}; priors={}; replacement-axis={}; \
             alternatives={}; {}; docs={}",
            self.intended_fit.join(","),
            self.anti_fit.join(","),
            self.roles.join(","),
            self.stop_reasons,
            self.execution_characteristics.join(","),
            self.runtime_requirements.join(","),
            self.configuration_schema.join(","),
            self.author_priors.join(","),
            self.replacement_axis,
            self.alternatives.join(","),
            self.oracle.selector_label(),
            self.documentation.join(",")
        )
    }
}

/// A topology descriptor paired with the complete cognition-specific metadata.
///
/// The concise selector summary is also written into the underlying topology
/// descriptor, so the oracle waiver and replacement boundary survive insertion
/// into the topology catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CognitivePointDescriptor {
    topology: TopologyDescriptor,
    metadata: PointMetadata,
}

impl CognitivePointDescriptor {
    /// Returns the descriptor accepted by the topology catalog.
    #[must_use]
    pub const fn topology(&self) -> &TopologyDescriptor {
        &self.topology
    }

    /// Consumes this value and returns the topology-catalog descriptor.
    #[must_use]
    pub fn into_topology(self) -> TopologyDescriptor {
        self.topology
    }

    /// Returns the structured point metadata used by registration and review.
    #[must_use]
    pub const fn metadata(&self) -> &PointMetadata {
        &self.metadata
    }
}

/// Immutable public handoff from private accumulators to embodiment resolvers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionInput<T> {
    assessments: Vec<Assessment<T>>,
    unvisited: Vec<T>,
    stop_reason: StopReason,
}

impl<T> ResolutionInput<T> {
    /// Creates an immutable accounting handoff from explicit bounded results.
    ///
    /// Model builders normally create this value from their private state. The
    /// constructor also supports direct routed embodiments that make the same
    /// terminal-accounting commitment without adopting either model shape.
    #[must_use]
    pub fn new(
        assessments: Vec<Assessment<T>>,
        unvisited: Vec<T>,
        stop_reason: StopReason,
    ) -> Self {
        Self {
            assessments,
            unvisited,
            stop_reason,
        }
    }

    /// Returns every assessment accumulated before terminal accounting.
    #[must_use]
    pub fn assessments(&self) -> &[Assessment<T>] {
        &self.assessments
    }

    /// Returns every scoped subject that was not visited before the bound.
    #[must_use]
    pub fn unvisited(&self) -> &[T] {
        &self.unvisited
    }

    /// Returns the semantic reason the bounded region stopped.
    #[must_use]
    pub const fn stop_reason(&self) -> StopReason {
        self.stop_reason
    }
}

/// Reusable native-graph cognition blocks.
pub mod blocks {
    use super::*;

    /// Role selection for a [`verified_step`] invocation.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerifiedRoles {
        /// A coverage probe followed by an independently observable judgement.
        Coverage,
        /// A correction producer followed by the existing verification role.
        Correction,
    }

    /// The four required builders for exhaustive [`Status`] routing.
    pub struct StatusArms {
        pass: GraphBuilder,
        issue: GraphBuilder,
        not_applicable: GraphBuilder,
        indeterminate: GraphBuilder,
    }

    impl StatusArms {
        /// Creates a total set of status arms. There is deliberately no default.
        #[must_use]
        pub fn new(
            pass: GraphBuilder,
            issue: GraphBuilder,
            not_applicable: GraphBuilder,
            indeterminate: GraphBuilder,
        ) -> Self {
            Self {
                pass,
                issue,
                not_applicable,
                indeterminate,
            }
        }
    }

    async fn route_pass() {}
    async fn route_issue() {}
    async fn route_not_applicable() {}
    async fn route_indeterminate() {}

    /// Adds a mandatory seed immediately followed by a native predicate loop.
    ///
    /// The ports are the typed seed, typed termination predicate, and inline
    /// body builder. The enclosing model declares the allowed [`StopReason`]
    /// set and residual policy; this shape adds no generic loop role. A direct
    /// native loop, a counted loop, or a fresh-context episode remains a
    /// replacement on the same episode-structure axis.
    pub fn seeded_loop<'g, T>(
        graph: &'g mut Graph,
        name: &'static str,
        seed: TypedSystem<T>,
        terminate: OutputPredicate<T>,
        body: GraphBuilder,
    ) -> &'g mut Graph
    where
        T: Output,
    {
        graph.system_boxed(seed).done();
        graph.add_loop::<T, _, _>(name, terminate, body)
    }

    /// Adds a status producer and a native four-case switch with no default.
    ///
    /// Each case begins with a private marker system, so an intentionally empty
    /// caller builder remains an explicit, valid case rather than disappearing.
    /// The switch has no role or termination policy of its own; every arm owns
    /// its visible continuation. A native two-arm conditional remains a valid
    /// replacement when the domain genuinely has a binary decision.
    pub fn status_dispatch(
        graph: &mut Graph,
        status: TypedSystem<Status>,
        arms: StatusArms,
    ) -> &mut Graph {
        status_dispatch_boxed(graph, status.into(), arms)
    }

    pub(crate) fn status_dispatch_boxed(
        graph: &mut Graph,
        status: BoxedSystem,
        arms: StatusArms,
    ) -> &mut Graph {
        let StatusArms {
            pass,
            issue,
            not_applicable,
            indeterminate,
        } = arms;

        graph.system_boxed(status).done();
        graph.add_switch::<Status, _, _, GraphBuilder>(
            "status_dispatch",
            |status| match status {
                Status::Pass => "pass",
                Status::Issue => "issue",
                Status::NotApplicable => "not-applicable",
                Status::Indeterminate => "indeterminate",
            },
            vec![
                (
                    "pass",
                    Box::new(move |branch: &mut Graph| {
                        branch.add_system(route_pass);
                        pass(branch);
                    }) as GraphBuilder,
                ),
                (
                    "issue",
                    Box::new(move |branch: &mut Graph| {
                        branch.add_system(route_issue);
                        issue(branch);
                    }) as GraphBuilder,
                ),
                (
                    "not-applicable",
                    Box::new(move |branch: &mut Graph| {
                        branch.add_system(route_not_applicable);
                        not_applicable(branch);
                    }) as GraphBuilder,
                ),
                (
                    "indeterminate",
                    Box::new(move |branch: &mut Graph| {
                        branch.add_system(route_indeterminate);
                        indeterminate(branch);
                    }) as GraphBuilder,
                ),
            ],
            None,
        )
    }

    /// Adds a producer followed by an independently scheduled assessment system.
    ///
    /// Construction requires registration-time evidence that the oracle rejects
    /// a known negative, or an explicit versioned waiver. The qualification is
    /// retained in the model descriptor rather than represented as a graph edge.
    /// The selected [`VerifiedRoles`] makes the producer/judge roles explicit;
    /// termination and residual handling stay with the enclosing model. A fused
    /// produce-and-judge system is the documented replacement when the oracle is
    /// inseparable from the act.
    pub fn verified_step<'g, Produced, Subject>(
        graph: &'g mut Graph,
        producer: TypedSystem<Produced>,
        judge: TypedSystem<Assessment<Subject>>,
        _qualification: &OracleQualification,
        roles: VerifiedRoles,
    ) -> &'g mut Graph
    where
        Produced: Output,
        Subject: Output,
    {
        match roles {
            VerifiedRoles::Coverage => {
                graph.system_boxed(producer).with_schedules(OnProbe).done();
                graph
                    .system_boxed(judge)
                    .with_schedules(OnAdjudicate)
                    .done();
            }
            VerifiedRoles::Correction => {
                graph.system_boxed(producer).done();
                graph.system_boxed(judge).with_schedules(OnVerify).done();
            }
        }
        graph
    }

    /// Adds one resolver and its accounting role after a bounded region.
    ///
    /// The resolver consumes the enclosing model's accumulated assessment and
    /// unvisited-work ports and must emit one immutable conclusion. Different
    /// named disposition policies replace one another behind this block; silent
    /// residual discard is never an alternative. `OnEscalate` is added only to
    /// a resolver that emits an escalated conclusion.
    pub fn resolve_residuals(
        graph: &mut Graph,
        resolver: TypedSystem<AssessmentConclusion>,
        emits_escalated_conclusion: bool,
    ) -> &mut Graph {
        let builder = graph.system_boxed(resolver).with_schedules(OnResolve);
        if emits_escalated_conclusion {
            builder.with_schedules(OnEscalate).done()
        } else {
            builder.done()
        }
    }

    /// Adds an inline, seeded worklist drain with a typed exhaustion predicate.
    ///
    /// The caller owns the scope, selection, and recording systems while the
    /// model that composes them keeps its concrete worklist and ledger private.
    /// Early-stop residuals must be exposed to [`resolve_residuals`].
    /// The enclosing model assigns its scope/probe/adjudication roles and reports
    /// work exhaustion or a configured bound. Fixed-arity, sampled, budgeted,
    /// and bounded-parallel sweeps remain replaceable alternatives.
    pub fn drain_worklist<'g, Progress>(
        graph: &'g mut Graph,
        name: &'static str,
        seed: TypedSystem<Progress>,
        terminate: OutputPredicate<Progress>,
        body: GraphBuilder,
    ) -> &'g mut Graph
    where
        Progress: Output,
    {
        seeded_loop(graph, name, seed, terminate, body)
    }

    pub(crate) fn drain_worklist_boxed<'g, Progress>(
        graph: &'g mut Graph,
        name: &'static str,
        seed: BoxedSystem,
        terminate: OutputPredicate<Progress>,
        body: GraphBuilder,
    ) -> &'g mut Graph
    where
        Progress: Output,
    {
        seeded_loop_boxed(graph, name, seed, terminate, body)
    }

    pub(crate) fn seeded_loop_boxed<'g, Progress>(
        graph: &'g mut Graph,
        name: &'static str,
        seed: BoxedSystem,
        terminate: OutputPredicate<Progress>,
        body: GraphBuilder,
    ) -> &'g mut Graph
    where
        Progress: Output,
    {
        graph.system_boxed(seed).done();
        graph.add_loop::<Progress, _, _>(name, terminate, body)
    }
}

#[derive(Debug)]
struct Worklist<T> {
    pending: VecDeque<T>,
    capacity: usize,
}
impl<T: Send + Sync + 'static> LocalResource for Worklist<T> {}

#[derive(Debug)]
struct Ledger<T> {
    assessments: Vec<Assessment<T>>,
    capacity: usize,
}
impl<T: Send + Sync + 'static> LocalResource for Ledger<T> {}

#[derive(Debug)]
struct SweepControl<T> {
    iteration_bound: usize,
    visited: usize,
    configured_stop: StopReason,
    subject: PhantomData<fn() -> T>,
}
impl<T: Send + Sync + 'static> LocalResource for SweepControl<T> {}

#[derive(Debug, Clone)]
struct SweepProgress {
    remaining: usize,
    visited: usize,
    iteration_bound: usize,
}

impl SweepProgress {
    fn complete(&self) -> bool {
        self.remaining == 0 || self.visited >= self.iteration_bound
    }
}

#[derive(Debug)]
struct CorrectionControl<T> {
    round_bound: usize,
    rounds: usize,
    accepted: bool,
    handed_outward: bool,
    subject: PhantomData<fn() -> T>,
}
impl<T: Send + Sync + 'static> LocalResource for CorrectionControl<T> {}

#[derive(Debug, Clone)]
struct CorrectionProgress {
    rounds: usize,
    round_bound: usize,
    accepted: bool,
    handed_outward: bool,
}

impl CorrectionProgress {
    fn complete(&self) -> bool {
        self.accepted || self.handed_outward || self.rounds >= self.round_bound
    }
}

/// Namespace for approved-block boundaries shared by more than one point.
const SHARED_BOUNDARY_NAMESPACE: &str = "cosmos.approved";

/// Behaviour version of the approved-block boundaries carried in that namespace.
const SHARED_BOUNDARY_VERSION: Version = Version::new(0, 1, 0);

/// Derives one subject-parameterized boundary key under `namespace`.
///
/// The subject's own stable key discriminates the derived key, so two subject
/// types never collide and one subject type always yields the same key.
fn derived_boundary_key(
    namespace: &str,
    field: &str,
    subject: &ContextTypeKey,
) -> Result<ContextTypeKey, CognitionError> {
    let id =
        ContextTypeId::new(format!("{namespace}.{field}.{}", subject.id())).map_err(|error| {
            CognitionError::new(format!(
                "derived boundary key is not a stable name: {error}"
            ))
        })?;
    Ok(ContextTypeKey::new(id, subject.version()))
}

/// Derives one boundary key for a type that carries no subject parameter.
fn fixed_boundary_key(namespace: &str, field: &str, version: Version) -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new(format!("{namespace}.{field}"))
            .expect("approved boundary key literal is a stable name"),
        version,
    )
}

/// Reports a binding that disagrees with one the registry already holds.
fn boundary_conflict(error: AdmissionBuildError) -> CognitionError {
    CognitionError::new(format!(
        "approved boundary key conflicts with an existing binding: {error}"
    ))
}

/// Explains the assessment ledger, which both approved points reuse.
fn bind_shared_ledger<Subject>(
    registry: &mut TypeExplanationRegistry,
    subject: &ContextTypeKey,
) -> Result<(), CognitionError>
where
    Subject: Send + Sync + 'static,
{
    registry
        .bind_resource::<Ledger<Subject>>(shared_ledger_key(subject)?)
        .map_err(boundary_conflict)
}

/// Returns the shared assessment-ledger key for `subject`.
fn shared_ledger_key(subject: &ContextTypeKey) -> Result<ContextTypeKey, CognitionError> {
    derived_boundary_key(SHARED_BOUNDARY_NAMESPACE, "assessment-ledger", subject)
}

/// Returns the shared resolution-input key for `subject`.
fn shared_resolution_input_key(subject: &ContextTypeKey) -> Result<ContextTypeKey, CognitionError> {
    derived_boundary_key(SHARED_BOUNDARY_NAMESPACE, "resolution-input", subject)
}

struct InitializeSweep<T>(PhantomData<fn() -> T>);

impl<T> InitializeSweep<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for InitializeSweep<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = SweepProgress;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let subjects = context.get_output::<Vec<T>>()?;
            let mut worklist = context.get_resource_mut::<Worklist<T>>()?;
            let mut ledger = context.get_resource_mut::<Ledger<T>>()?;
            let mut control = context.get_resource_mut::<SweepControl<T>>()?;
            if subjects.len() > worklist.capacity {
                return Err(SystemError::ExecutionError(format!(
                    "scope produced {} subjects, exceeding configured worklist capacity {}",
                    subjects.len(),
                    worklist.capacity
                )));
            }
            worklist.pending = subjects.iter().cloned().collect();
            ledger.assessments.clear();
            control.visited = 0;
            Ok(SweepProgress {
                remaining: worklist.pending.len(),
                visited: 0,
                iteration_bound: control.iteration_bound,
            })
        })
    }

    fn name(&self) -> &'static str {
        "initialize_sweep"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_output::<Vec<T>>()
            .with_write::<Worklist<T>>()
            .with_write::<Ledger<T>>()
            .with_write::<SweepControl<T>>()
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

struct SweepSeed<T>(PhantomData<fn() -> T>);

impl<T> SweepSeed<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for SweepSeed<T>
where
    T: Send + Sync + 'static,
{
    type Output = SweepProgress;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let worklist = context.get_resource::<Worklist<T>>()?;
            let control = context.get_resource::<SweepControl<T>>()?;
            Ok(SweepProgress {
                remaining: worklist.pending.len(),
                visited: control.visited,
                iteration_bound: control.iteration_bound,
            })
        })
    }

    fn name(&self) -> &'static str {
        "sweep_seed"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_read::<Worklist<T>>()
            .with_read::<SweepControl<T>>()
    }
}

struct SelectSubject<T>(PhantomData<fn() -> T>);

impl<T> SelectSubject<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for SelectSubject<T>
where
    T: Send + Sync + 'static,
{
    type Output = T;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let mut worklist = context.get_resource_mut::<Worklist<T>>()?;
            worklist.pending.pop_front().ok_or_else(|| {
                SystemError::ExecutionError("worklist exhausted before selection".to_owned())
            })
        })
    }

    fn name(&self) -> &'static str {
        "select_subject"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new().with_write::<Worklist<T>>()
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

struct RecordAssessment<T>(PhantomData<fn() -> T>);

impl<T> RecordAssessment<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for RecordAssessment<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = SweepProgress;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let assessment = context.get_output::<Assessment<T>>()?;
            let worklist = context.get_resource::<Worklist<T>>()?;
            let mut ledger = context.get_resource_mut::<Ledger<T>>()?;
            let mut control = context.get_resource_mut::<SweepControl<T>>()?;
            if ledger.assessments.len() >= ledger.capacity {
                return Err(SystemError::ExecutionError(format!(
                    "assessment ledger exceeded configured capacity {}",
                    ledger.capacity
                )));
            }
            ledger.assessments.push(assessment.clone());
            control.visited += 1;
            Ok(SweepProgress {
                remaining: worklist.pending.len(),
                visited: control.visited,
                iteration_bound: control.iteration_bound,
            })
        })
    }

    fn name(&self) -> &'static str {
        "record_assessment"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_output::<Assessment<T>>()
            .with_read::<Worklist<T>>()
            .with_write::<Ledger<T>>()
            .with_write::<SweepControl<T>>()
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

struct PendingSubjects<T>(PhantomData<fn() -> T>);

impl<T> PendingSubjects<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for PendingSubjects<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = Vec<T>;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let worklist = context.get_resource::<Worklist<T>>()?;
            Ok(worklist.pending.iter().cloned().collect())
        })
    }

    fn name(&self) -> &'static str {
        "pending_subjects"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new().with_read::<Worklist<T>>()
    }
}

struct AppendGapAssessments<T>(PhantomData<fn() -> T>);

impl<T> AppendGapAssessments<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for AppendGapAssessments<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = ();

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let gaps = context.get_output::<Vec<Assessment<T>>>()?;
            let pending = context.get_output::<Vec<T>>()?;
            let mut ledger = context.get_resource_mut::<Ledger<T>>()?;
            if gaps.len() != pending.len() {
                return Err(SystemError::ExecutionError(format!(
                    "gap assessor produced {} assessments for {} unvisited subjects",
                    gaps.len(),
                    pending.len()
                )));
            }
            if ledger.assessments.len() + gaps.len() > ledger.capacity {
                return Err(SystemError::ExecutionError(format!(
                    "gap assessments exceed configured ledger capacity {}",
                    ledger.capacity
                )));
            }
            ledger.assessments.extend(gaps.iter().cloned());
            Ok(())
        })
    }

    fn name(&self) -> &'static str {
        "append_gap_assessments"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_output::<Vec<Assessment<T>>>()
            .with_output::<Vec<T>>()
            .with_write::<Ledger<T>>()
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

struct SweepResolutionInput<T>(PhantomData<fn() -> T>);

impl<T> SweepResolutionInput<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for SweepResolutionInput<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = ResolutionInput<T>;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let worklist = context.get_resource::<Worklist<T>>()?;
            let ledger = context.get_resource::<Ledger<T>>()?;
            let control = context.get_resource::<SweepControl<T>>()?;
            Ok(ResolutionInput {
                assessments: ledger.assessments.clone(),
                unvisited: worklist.pending.iter().cloned().collect(),
                stop_reason: if worklist.pending.is_empty() {
                    StopReason::WorkExhausted
                } else {
                    control.configured_stop
                },
            })
        })
    }

    fn name(&self) -> &'static str {
        "sweep_resolution_input"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_read::<Worklist<T>>()
            .with_read::<Ledger<T>>()
            .with_read::<SweepControl<T>>()
    }
}

struct SeedCorrection<T>(PhantomData<fn() -> T>);

impl<T> SeedCorrection<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for SeedCorrection<T>
where
    T: Send + Sync + 'static,
{
    type Output = CorrectionProgress;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let control = context.get_resource::<CorrectionControl<T>>()?;
            Ok(CorrectionProgress {
                rounds: control.rounds,
                round_bound: control.round_bound,
                accepted: control.accepted,
                handed_outward: control.handed_outward,
            })
        })
    }

    fn name(&self) -> &'static str {
        "seed_correction"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new().with_read::<CorrectionControl<T>>()
    }
}

struct RecordCorrection<T>(PhantomData<fn() -> T>);

impl<T> RecordCorrection<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for RecordCorrection<T>
where
    T: Clone + Serialize + Send + Sync + 'static,
{
    type Output = Status;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let assessment = context.get_output::<Assessment<T>>()?;
            let mut ledger = context.get_resource_mut::<Ledger<T>>()?;
            let mut control = context.get_resource_mut::<CorrectionControl<T>>()?;
            if ledger.assessments.len() >= ledger.capacity {
                return Err(SystemError::ExecutionError(format!(
                    "correction ledger exceeded configured capacity {}",
                    ledger.capacity
                )));
            }
            let status = assessment.status();
            ledger.assessments.push(assessment.clone());
            control.rounds += 1;
            match status {
                Status::Pass => control.accepted = true,
                Status::Indeterminate => control.handed_outward = true,
                Status::Issue | Status::NotApplicable => {}
            }
            Ok(status)
        })
    }

    fn name(&self) -> &'static str {
        "record_correction"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_output::<Assessment<T>>()
            .with_write::<Ledger<T>>()
            .with_write::<CorrectionControl<T>>()
    }

    fn is_fallible(&self) -> bool {
        true
    }
}

struct CorrectionProgressSystem<T>(PhantomData<fn() -> T>);

impl<T> CorrectionProgressSystem<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for CorrectionProgressSystem<T>
where
    T: Send + Sync + 'static,
{
    type Output = CorrectionProgress;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let control = context.get_resource::<CorrectionControl<T>>()?;
            Ok(CorrectionProgress {
                rounds: control.rounds,
                round_bound: control.round_bound,
                accepted: control.accepted,
                handed_outward: control.handed_outward,
            })
        })
    }

    fn name(&self) -> &'static str {
        "correction_progress"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new().with_read::<CorrectionControl<T>>()
    }
}

struct CorrectionResolutionInput<T>(PhantomData<fn() -> T>);

impl<T> CorrectionResolutionInput<T> {
    fn new() -> Self {
        Self(PhantomData)
    }
}

impl<T> System for CorrectionResolutionInput<T>
where
    T: Clone + Send + Sync + 'static,
{
    type Output = ResolutionInput<T>;

    fn run<'a>(
        &'a self,
        context: &'a SystemContext<'_>,
    ) -> BoxFuture<'a, Result<Self::Output, SystemError>> {
        Box::pin(async move {
            let ledger = context.get_resource::<Ledger<T>>()?;
            let control = context.get_resource::<CorrectionControl<T>>()?;
            Ok(ResolutionInput {
                assessments: ledger.assessments.clone(),
                unvisited: Vec::new(),
                stop_reason: if control.handed_outward {
                    StopReason::EscalatedOut
                } else if control.accepted {
                    StopReason::PredicateSatisfied
                } else {
                    StopReason::BudgetExhausted
                },
            })
        })
    }

    fn name(&self) -> &'static str {
        "correction_resolution_input"
    }

    fn access(&self) -> SystemAccess {
        SystemAccess::new()
            .with_read::<Ledger<T>>()
            .with_read::<CorrectionControl<T>>()
    }
}

/// Systematic sequential coverage over an embodiment-owned bounded subject set.
pub mod coverage_sweep {
    use super::*;
    use blocks::{VerifiedRoles, drain_worklist_boxed, resolve_residuals, verified_step};

    /// Stable catalog identity of this non-canonical coverage point.
    pub const TOPOLOGY_ID: &str = "cosmos.coverage-sweep";

    /// Behaviour version of the coverage point.
    pub const TOPOLOGY_VERSION: Version = Version::new(0, 1, 0);

    /// Runtime bounds for one coverage episode.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Config {
        subject_capacity: usize,
        assessment_capacity: usize,
        iteration_bound: usize,
        bounded_stop: StopReason,
    }

    impl Config {
        /// Creates non-zero bounds and chooses the truthful early-bound reason.
        pub fn new(
            subject_capacity: usize,
            assessment_capacity: usize,
            iteration_bound: usize,
            bounded_stop: StopReason,
        ) -> Result<Self, CognitionError> {
            if subject_capacity == 0 || assessment_capacity == 0 || iteration_bound == 0 {
                return Err(CognitionError::new("coverage bounds must be non-zero"));
            }
            if assessment_capacity < subject_capacity {
                return Err(CognitionError::new(
                    "assessment capacity must cover every scoped subject",
                ));
            }
            if !matches!(
                bounded_stop,
                StopReason::BudgetExhausted | StopReason::IterationLimit
            ) {
                return Err(CognitionError::new(
                    "coverage bound must report budget-exhausted or iteration-limit",
                ));
            }
            Ok(Self {
                subject_capacity,
                assessment_capacity,
                iteration_bound,
                bounded_stop,
            })
        }
    }

    /// Typed embodiment systems occupying the coverage ports.
    pub struct Stages<Subject, Observation>
    where
        Subject: Output,
        Observation: Output,
    {
        /// Discovers the complete bounded subject set.
        pub scope: TypedSystem<Vec<Subject>>,
        /// Produces an evidence-bearing assessment for an empty scope.
        pub empty_scope: TypedSystem<Assessment<Subject>>,
        /// Observes the currently selected subject.
        pub probe: TypedSystem<Observation>,
        /// Judges the observation and selected subject.
        pub adjudicate: TypedSystem<Assessment<Subject>>,
        /// Maps every unvisited subject to a visible gap assessment.
        pub assess_gaps: TypedSystem<Vec<Assessment<Subject>>>,
        /// Combines the complete immutable ledger under a named policy.
        pub combine: TypedSystem<AssessmentSummary>,
        /// Accounts for every residual and emits the conclusion.
        pub resolve: TypedSystem<AssessmentConclusion>,
    }

    /// Returns this model point's stable topology identity.
    #[must_use]
    pub fn topology_key() -> TopologyKey {
        TopologyKey::new(
            TopologyId::new(TOPOLOGY_ID).expect("coverage topology ID is valid"),
            TOPOLOGY_VERSION,
        )
    }

    /// Returns the complete selector-facing descriptor for one registration.
    #[must_use]
    pub fn descriptor(
        contract: TopologyContractKey,
        oracle: OracleQualification,
    ) -> CognitivePointDescriptor {
        let metadata = PointMetadata {
            intended_fit: &[
                "high independent-work cardinality",
                "systematic coverage required",
                "verifier available",
            ],
            anti_fit: &[
                "tight latency limit",
                "single-subject refinement",
                "no verifier capability",
            ],
            roles: &[
                "OnScope",
                "OnProbe",
                "OnAdjudicate",
                "OnCombine",
                "OnResolve",
            ],
            stop_reasons: &[
                StopReason::WorkExhausted,
                StopReason::BudgetExhausted,
                StopReason::IterationLimit,
                StopReason::CircuitBroken,
                StopReason::HumanStopped,
                StopReason::EscalatedOut,
            ],
            execution_characteristics: &[
                "one sequential loop",
                "cost scales linearly with subject count",
                "no concurrency",
            ],
            runtime_requirements: &[
                "native predicate loop",
                "native conditional branch",
                "mutable local accumulators",
                "no map-collect, suspension, recursion, or public edges",
            ],
            configuration_schema: &[
                "subject capacity",
                "assessment capacity",
                "iteration bound",
                "combination policy reference",
                "residual policy reference",
            ],
            author_priors: &[
                "coverage completeness: high, conditional on complete scope",
                "latency: linear in subject count",
                "token cost: linear in subject count",
            ],
            documentation: &[
                "docs/reference/cognitive-models.md#coverage_sweep",
                "tests/cognition_models.rs",
            ],
            replacement_axis: "systematic coverage policy",
            alternatives: &[
                "fixed-arity sweep",
                "sampled sweep",
                "budgeted sweep",
                "bounded parallel sweep when supported",
            ],
            oracle,
        };
        let summary = metadata.selector_summary("sequential exhaustive coverage");
        let topology = TopologyDescriptor::new(topology_key(), "Coverage sweep", summary)
            .expect("coverage descriptor metadata is valid")
            .claim(contract);
        CognitivePointDescriptor { topology, metadata }
    }

    /// Installs fresh private worklist and ledger state into a run context.
    ///
    /// A context may host only one coverage episode for a given `Subject` type
    /// at a time. Reusing it requires a fresh child or root context.
    pub fn prepare_context<Subject>(
        context: &mut SystemContext<'_>,
        config: Config,
    ) -> Result<(), CognitionError>
    where
        Subject: Send + Sync + 'static,
    {
        if context.contains_local_resource::<Worklist<Subject>>()
            || context.contains_local_resource::<Ledger<Subject>>()
            || context.contains_local_resource::<SweepControl<Subject>>()
        {
            return Err(CognitionError::new(
                "coverage state is already installed in this context",
            ));
        }
        context.insert(Worklist::<Subject> {
            pending: VecDeque::with_capacity(config.subject_capacity),
            capacity: config.subject_capacity,
        });
        context.insert(Ledger::<Subject> {
            assessments: Vec::with_capacity(config.assessment_capacity),
            capacity: config.assessment_capacity,
        });
        context.insert(SweepControl::<Subject> {
            iteration_bound: config.iteration_bound,
            visited: 0,
            configured_stop: config.bounded_stop,
            subject: PhantomData,
        });
        Ok(())
    }

    /// Returns the stable keys of the state [`prepare_context`] installs.
    ///
    /// A host reports these as writable resources in its runtime availability
    /// snapshot. Without those facts an otherwise explained coverage
    /// registration is excluded by `resource.missing`.
    pub fn state_resource_keys(
        subject: &ContextTypeKey,
    ) -> Result<Vec<ContextTypeKey>, CognitionError> {
        Ok(vec![
            shared_ledger_key(subject)?,
            worklist_key(subject)?,
            control_key(subject)?,
        ])
    }

    /// Returns the worklist key for `subject`.
    fn worklist_key(subject: &ContextTypeKey) -> Result<ContextTypeKey, CognitionError> {
        derived_boundary_key(SHARED_BOUNDARY_NAMESPACE, "worklist", subject)
    }

    /// Returns this point's sweep-control key for `subject`.
    fn control_key(subject: &ContextTypeKey) -> Result<ContextTypeKey, CognitionError> {
        derived_boundary_key(TOPOLOGY_ID, "control", subject)
    }

    /// Explains every graph boundary this point introduces, keyed by `subject`.
    ///
    /// An embodiment names and binds its own subject, observation, assessment,
    /// summary, and conclusion types. It cannot name the worklist, ledger,
    /// control, or progress types, which are private to Cosmos: Cosmos
    /// explains the boundaries Cosmos introduces. Skipping this call leaves
    /// those boundaries unexplained, and preflight then excludes the
    /// registration with `type-explanation.missing` so it can never be
    /// selected.
    ///
    /// Binding is idempotent, and the keys shared with
    /// [`bounded_correction::register_type_explanations`] are identical, so
    /// registering both points against one subject is safe.
    pub fn register_type_explanations<Subject>(
        registry: &mut TypeExplanationRegistry,
        subject: &ContextTypeKey,
    ) -> Result<(), CognitionError>
    where
        Subject: Send + Sync + 'static,
    {
        bind_shared_ledger::<Subject>(registry, subject)?;
        registry
            .bind_resource::<Worklist<Subject>>(worklist_key(subject)?)
            .map_err(boundary_conflict)?;
        registry
            .bind_resource::<SweepControl<Subject>>(control_key(subject)?)
            .map_err(boundary_conflict)?;
        registry
            .bind_output::<SweepProgress>(fixed_boundary_key(
                TOPOLOGY_ID,
                "progress",
                TOPOLOGY_VERSION,
            ))
            .map_err(boundary_conflict)?;
        registry
            .bind_output::<ResolutionInput<Subject>>(shared_resolution_input_key(subject)?)
            .map_err(boundary_conflict)
    }

    /// Returns the minimum structural contract for this embodiment boundary.
    #[must_use]
    pub fn contract_signature<Subject, Observation>() -> GraphSignature
    where
        Subject: Output,
        Observation: Output,
    {
        GraphSignature::new()
            .require_write::<Worklist<Subject>>()
            .require_write::<Ledger<Subject>>()
            .require_write::<SweepControl<Subject>>()
            .require_output::<Subject>()
            .require_output::<Observation>()
            .require_output::<Assessment<Subject>>()
            .require_output::<Vec<Subject>>()
            .require_output::<Vec<Assessment<Subject>>>()
            .produce::<ResolutionInput<Subject>>()
            .produce::<AssessmentSummary>()
            .produce::<AssessmentConclusion>()
    }

    /// Builds the approved coverage point into `graph`.
    pub fn build<Subject, Observation>(
        graph: &mut Graph,
        stages: Stages<Subject, Observation>,
        qualification: OracleQualification,
    ) -> &mut Graph
    where
        Subject: Clone + Send + Sync + 'static,
        Observation: Output,
    {
        let Stages {
            scope,
            empty_scope,
            probe,
            adjudicate,
            assess_gaps,
            combine,
            resolve,
        } = stages;

        graph.system_boxed(scope).with_schedules(OnScope).done();
        graph
            .system_boxed(Box::new(InitializeSweep::<Subject>::new()))
            .done();
        graph.add_conditional_branch::<SweepProgress, _, _, _>(
            "empty_scope",
            |progress| progress.remaining == 0,
            move |empty| {
                empty
                    .system_boxed(empty_scope)
                    .with_schedules(OnAdjudicate)
                    .done();
                empty
                    .system_boxed(Box::new(RecordAssessment::<Subject>::new()))
                    .done();
            },
            move |non_empty| {
                drain_worklist_boxed(
                    non_empty,
                    "coverage_sweep",
                    Box::new(SweepSeed::<Subject>::new()),
                    Box::new(SweepProgress::complete),
                    Box::new(move |body| {
                        body.system_boxed(Box::new(SelectSubject::<Subject>::new()))
                            .done();
                        verified_step(
                            body,
                            probe,
                            adjudicate,
                            &qualification,
                            VerifiedRoles::Coverage,
                        );
                        body.system_boxed(Box::new(RecordAssessment::<Subject>::new()))
                            .done();
                    }),
                );
                non_empty
                    .system_boxed(Box::new(PendingSubjects::<Subject>::new()))
                    .done();
                non_empty.system_boxed(assess_gaps).done();
                non_empty
                    .system_boxed(Box::new(AppendGapAssessments::<Subject>::new()))
                    .done();
            },
        );
        graph
            .system_boxed(Box::new(SweepResolutionInput::<Subject>::new()))
            .done();
        graph.system_boxed(combine).with_schedules(OnCombine).done();
        resolve_residuals(graph, resolve, false)
    }
}

/// Bounded iterative correction with exhaustive assessment-status routing.
pub mod bounded_correction {
    use super::*;
    use blocks::{
        StatusArms, VerifiedRoles, resolve_residuals, seeded_loop_boxed, status_dispatch_boxed,
        verified_step,
    };

    /// Stable catalog identity of this non-canonical correction point.
    pub const TOPOLOGY_ID: &str = "cosmos.bounded-correction";

    /// Behaviour version of the correction point.
    pub const TOPOLOGY_VERSION: Version = Version::new(0, 1, 0);

    /// Runtime bounds for one correction episode.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Config {
        round_bound: usize,
        assessment_capacity: usize,
    }

    impl Config {
        /// Creates non-zero round and ledger bounds.
        pub fn new(round_bound: usize, assessment_capacity: usize) -> Result<Self, CognitionError> {
            if round_bound == 0 || assessment_capacity < round_bound {
                return Err(CognitionError::new(
                    "correction requires a non-zero round bound covered by ledger capacity",
                ));
            }
            Ok(Self {
                round_bound,
                assessment_capacity,
            })
        }
    }

    /// Typed embodiment systems occupying the correction ports.
    pub struct Stages<Subject>
    where
        Subject: Output,
    {
        /// Creates or revises the subject for the current round.
        pub produce: TypedSystem<Subject>,
        /// Judges the current subject under an evidence-bearing oracle.
        pub judge: TypedSystem<Assessment<Subject>>,
        /// Combines every round assessment under a named policy.
        pub combine: TypedSystem<AssessmentSummary>,
        /// Resolves a non-escalated terminal episode.
        pub resolve: TypedSystem<AssessmentConclusion>,
        /// Resolves and emits an escalated terminal episode.
        pub escalate: TypedSystem<AssessmentConclusion>,
    }

    /// Returns this model point's stable topology identity.
    #[must_use]
    pub fn topology_key() -> TopologyKey {
        TopologyKey::new(
            TopologyId::new(TOPOLOGY_ID).expect("correction topology ID is valid"),
            TOPOLOGY_VERSION,
        )
    }

    /// Returns the complete selector-facing descriptor for one registration.
    #[must_use]
    pub fn descriptor(
        contract: TopologyContractKey,
        oracle: OracleQualification,
    ) -> CognitivePointDescriptor {
        let metadata = PointMetadata {
            intended_fit: &[
                "high refinement need",
                "single-subject cardinality",
                "verifier available",
                "moderate uncertainty",
            ],
            anti_fit: &[
                "high independent-work cardinality",
                "tight latency limit",
                "no refinement need",
            ],
            roles: &["OnVerify", "OnResolve", "OnEscalate"],
            stop_reasons: &[
                StopReason::PredicateSatisfied,
                StopReason::GoalMet,
                StopReason::BudgetExhausted,
                StopReason::IterationLimit,
                StopReason::EscalatedOut,
                StopReason::CircuitBroken,
                StopReason::HumanStopped,
            ],
            execution_characteristics: &[
                "one sequential loop",
                "cost scales linearly with round count",
                "no concurrency",
                "producer side effects remain embodiment-declared",
            ],
            runtime_requirements: &[
                "native predicate loop",
                "native closed-key switch",
                "one mutable local accumulator",
                "no slots, suspension, map-collect, or context replacement",
            ],
            configuration_schema: &[
                "round bound",
                "assessment capacity",
                "acceptance policy reference",
                "issue-continuation policy reference",
                "residual policy reference",
            ],
            author_priors: &[
                "convergence quality: moderate-to-high, conditional on oracle",
                "latency: linear in round count",
                "token cost: linear in round count",
            ],
            documentation: &[
                "docs/reference/cognitive-models.md#bounded_correction",
                "tests/cognition_models.rs",
            ],
            replacement_axis: "bounded iterative correction policy",
            alternatives: &[
                "prediction-error loop",
                "fresh-context correction when supported",
                "ensemble-judged correction when supported",
            ],
            oracle,
        };
        let summary = metadata.selector_summary("bounded iterative correction");
        let topology = TopologyDescriptor::new(topology_key(), "Bounded correction", summary)
            .expect("correction descriptor metadata is valid")
            .claim(contract);
        CognitivePointDescriptor { topology, metadata }
    }

    /// Installs fresh private round and ledger state into a run context.
    pub fn prepare_context<Subject>(
        context: &mut SystemContext<'_>,
        config: Config,
    ) -> Result<(), CognitionError>
    where
        Subject: Send + Sync + 'static,
    {
        if context.contains_local_resource::<Ledger<Subject>>()
            || context.contains_local_resource::<CorrectionControl<Subject>>()
        {
            return Err(CognitionError::new(
                "correction state is already installed in this context",
            ));
        }
        context.insert(Ledger::<Subject> {
            assessments: Vec::with_capacity(config.assessment_capacity),
            capacity: config.assessment_capacity,
        });
        context.insert(CorrectionControl::<Subject> {
            round_bound: config.round_bound,
            rounds: 0,
            accepted: false,
            handed_outward: false,
            subject: PhantomData,
        });
        Ok(())
    }

    /// Returns the stable keys of the state [`prepare_context`] installs.
    ///
    /// A host reports these as writable resources in its runtime availability
    /// snapshot. Without those facts an otherwise explained correction
    /// registration is excluded by `resource.missing`.
    pub fn state_resource_keys(
        subject: &ContextTypeKey,
    ) -> Result<Vec<ContextTypeKey>, CognitionError> {
        Ok(vec![shared_ledger_key(subject)?, control_key(subject)?])
    }

    /// Returns this point's correction-control key for `subject`.
    fn control_key(subject: &ContextTypeKey) -> Result<ContextTypeKey, CognitionError> {
        derived_boundary_key(TOPOLOGY_ID, "control", subject)
    }

    /// Explains every graph boundary this point introduces, keyed by `subject`.
    ///
    /// An embodiment names and binds its own subject, assessment, summary, and
    /// conclusion types. It cannot name the ledger, control, or progress
    /// types, and the routed [`Status`] boundary exists only because this
    /// point dispatches on it: Cosmos explains the boundaries Cosmos
    /// introduces. Skipping this call leaves those boundaries unexplained, and
    /// preflight then excludes the registration with
    /// `type-explanation.missing` so it can never be selected.
    ///
    /// Binding is idempotent, and the keys shared with
    /// [`coverage_sweep::register_type_explanations`] are identical, so
    /// registering both points against one subject is safe.
    pub fn register_type_explanations<Subject>(
        registry: &mut TypeExplanationRegistry,
        subject: &ContextTypeKey,
    ) -> Result<(), CognitionError>
    where
        Subject: Send + Sync + 'static,
    {
        bind_shared_ledger::<Subject>(registry, subject)?;
        registry
            .bind_resource::<CorrectionControl<Subject>>(control_key(subject)?)
            .map_err(boundary_conflict)?;
        registry
            .bind_output::<CorrectionProgress>(fixed_boundary_key(
                TOPOLOGY_ID,
                "progress",
                TOPOLOGY_VERSION,
            ))
            .map_err(boundary_conflict)?;
        registry
            .bind_output::<Status>(fixed_boundary_key(
                SHARED_BOUNDARY_NAMESPACE,
                "assessment-status",
                SHARED_BOUNDARY_VERSION,
            ))
            .map_err(boundary_conflict)?;
        registry
            .bind_output::<ResolutionInput<Subject>>(shared_resolution_input_key(subject)?)
            .map_err(boundary_conflict)
    }

    /// Returns the minimum structural contract for this embodiment boundary.
    #[must_use]
    pub fn contract_signature<Subject>() -> GraphSignature
    where
        Subject: Output,
    {
        GraphSignature::new()
            .require_write::<Ledger<Subject>>()
            .require_write::<CorrectionControl<Subject>>()
            .require_output::<Subject>()
            .require_output::<Assessment<Subject>>()
            .produce::<ResolutionInput<Subject>>()
            .produce::<AssessmentSummary>()
            .produce::<AssessmentConclusion>()
    }

    /// Builds the approved correction point into `graph`.
    pub fn build<Subject>(
        graph: &mut Graph,
        stages: Stages<Subject>,
        qualification: OracleQualification,
    ) -> &mut Graph
    where
        Subject: Clone + Serialize + Send + Sync + 'static,
    {
        let Stages {
            produce,
            judge,
            combine,
            resolve,
            escalate,
        } = stages;

        seeded_loop_boxed(
            graph,
            "bounded_correction",
            Box::new(SeedCorrection::<Subject>::new()),
            Box::new(CorrectionProgress::complete),
            Box::new(move |body| {
                verified_step(
                    body,
                    produce,
                    judge,
                    &qualification,
                    VerifiedRoles::Correction,
                );
                status_dispatch_boxed(
                    body,
                    Box::new(RecordCorrection::<Subject>::new()),
                    StatusArms::new(
                        Box::new(|_| {}),
                        Box::new(|_| {}),
                        Box::new(|_| {}),
                        Box::new(|_| {}),
                    ),
                );
                body.system_boxed(Box::new(CorrectionProgressSystem::<Subject>::new()))
                    .done();
            }),
        );
        graph
            .system_boxed(Box::new(CorrectionResolutionInput::<Subject>::new()))
            .done();
        graph.system_boxed(combine).done();
        graph.add_conditional_branch::<ResolutionInput<Subject>, _, _, _>(
            "escalated_conclusion",
            |input| input.stop_reason() == StopReason::EscalatedOut,
            move |branch| {
                resolve_residuals(branch, escalate, true);
            },
            move |branch| {
                resolve_residuals(branch, resolve, false);
            },
        )
    }
}
