//! The coding topology pack and the host's observation of what it can supply.
//!
//! Cosmos owns the catalog *mechanism*; [`CodingTopologyPackPlugin`] owns the
//! coding *content*. The split matters at three points:
//!
//! - the [`contract`] is a native [`GraphSignature`], so structural
//!   satisfaction stays Polaris's job, not a Cosmos re-implementation;
//! - coding-owned boundary types and both reused Cosmos points' private
//!   boundaries are explained while the pack assembles each complete
//!   registration; and
//! - [`probe_availability`] reports what *this host* has wired, not what
//!   happens to exist on the machine — a test runner on `PATH` that the host
//!   never installed is not an available resource.

use std::path::Path;

use polaris::graph::{Graph, GraphSignature};
use polaris::models::ModelRegistry;
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};
use polaris_cosmos::cognition::approved::bounded_correction;
use polaris_cosmos::cognition::approved::coverage_sweep;
use polaris_cosmos::cognition::prediction_error_loop;
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, AssessmentSummary, AvailabilityObservation,
    AvailabilitySnapshotId, AvailabilityState, ConfigurationSchemaId, ConfigurationSchemaKey,
    ConfigurationValue, ContentDigest, ContextTypeId, ContextTypeKey, ContractError, DurationMs,
    EnvironmentId, EnvironmentKey, EpistemicBasis, EvidenceTime, OutputAvailability,
    ProviderAvailability, ProviderId, ProviderKey, RegistrationSelectionRef,
    ResolvedConfigurationRef, ResourceAccess, ResourceAvailability, RuntimeAvailabilitySnapshot,
    SourceKind,
};
use polaris_cosmos::topology::{
    AdmissionBuildError, CandidateRequirements, CompleteTopologyRegistration, IdentifierError,
    MetadataError, ProviderRequirement, RegistrationId, RegistrationProvenance, TopologyCatalog,
    TopologyCatalogError, TopologyContract, TopologyContractId, TopologyContractKey,
    TopologyDescriptor, TopologyId, TopologyKey, TypeExplanationRegistry,
};

use super::graphs;
use super::payload::{
    AppliedEdit, CodingQuestion, CodingWorkspace, EditFeedback, EditSite, PredictedEdit,
    ProjectTestRunner, QuestionFinding, SiteObservation, SweepProgress, SweepWorklist,
    VerifiedEdit,
};
use super::task::CodingTask;
use super::{behavior_version, policy, provenance};
use crate::usage::TokenMeter;

/// Registration that applies the edit in one pass.
pub const DIRECT: &str = "code.direct-edit";

/// Registration that commits a prediction, edits, and re-reads to check it.
pub const PREDICTION_ERROR: &str = "code.prediction-error-edit";

/// Registration that sweeps a bounded worklist of real stub sites.
pub const WORKLIST_SWEEP: &str = "code.worklist-sweep-edit";

/// Registration whose verifier is a project test run.
pub const TEST_GATED: &str = "code.test-gated-edit";

/// Registration whose edit is proposed by a language model.
pub const MODEL_PROPOSED: &str = "code.model-proposed-edit";

/// Registration built from Cosmos's shipped coverage sweep.
pub const COVERAGE_AUDIT: &str = "code.coverage-audit";

/// Registration that answers a bounded question about the tree and stops.
///
/// The only registration in this pack that writes nothing, and the only one
/// that requires nothing beyond a readable working directory — which is what
/// makes asking a question routable on an ordinary machine instead of
/// automatically ineligible.
pub const TREE_QUESTION: &str = "code.tree-question";

/// Stable identity of the one contract every coding registration claims.
pub const CONTRACT: &str = "polaris-code.coding-assessment";

/// Provider a model-proposed edit needs, and this host has not wired.
pub const MODEL_PROVIDER: &str = "polaris-code.anthropic";

/// Stable type key of the model registry a coding topology calls a model
/// through.
///
/// A pack that ships a model-calling graph reads `Res<ModelRegistry>` and must
/// bind this exact key, so the one coding vocabulary keeps naming one native
/// type. The host reports it as available only when the accepted task declared
/// [`HostCapabilities::model_provider`].
pub const MODEL_REGISTRY_TYPE: &str = "polaris-code.model-registry";

/// Stable type key of the per-run token meter a coding topology bills its own
/// model spend to.
///
/// [`select_and_run`](super::host::select_and_run) inserts a fresh
/// [`TokenMeter`] into every topology run context, where it shadows the
/// session's global meter. A graph that reads `Res<TokenMeter>` therefore bills
/// the run, never the session.
pub const RUN_METER_TYPE: &str = "polaris-code.run-token-meter";

/// Contributes all coding graphs and their immutable admission metadata to the
/// process-wide topology catalog.
///
/// Add this plugin when a host should discover the built-in coding topologies
/// through the same catalog capability as every other compiled-in pack. Adding
/// a topology changes this pack only; host assembly still adds one plugin.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | Extends the catalog provider's build-phase resource rather than providing another resource. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | The sealed catalog is consumed through `Res<TopologyCatalog>`. |
///
/// # Dependencies
///
/// Requires a plugin providing [`TopologyCatalog`], normally
/// [`TopologyCatalogPlugin`](polaris_cosmos::topology::TopologyCatalogPlugin).
///
/// # Lifecycle
///
/// During `build`, the pack registers one coding contract and every coding
/// graph as an atomic [`CompleteTopologyRegistration`]. The provider validates
/// and seals the combined catalog during `ready`.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code_core::cosmos::topology::CodingTopologyPackPlugin;
/// use polaris_cosmos::topology::TopologyCatalogPlugin;
///
/// # async fn assemble() {
/// let mut server = Server::new();
/// server
///     .add_plugins(TopologyCatalogPlugin)
///     .add_plugins(CodingTopologyPackPlugin);
/// server.finish().await.expect("coding topology pack should assemble");
/// # }
/// ```
#[derive(Debug, Default, Copy, Clone)]
pub struct CodingTopologyPackPlugin;

#[plugin(id = "polaris_code::coding_topology_pack", version = "0.1.0")]
impl Plugin for CodingTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        if let Err(error) = register_coding_topologies(&mut catalog) {
            panic!("coding topology pack registration failed: {error}");
        }
    }
}

/// Returns the coding pack's context type key for one stable type id.
///
/// A pack that ships a coding topology reading the model registry or the
/// per-run token meter binds [`MODEL_REGISTRY_TYPE`] and [`RUN_METER_TYPE`]
/// through this helper, so its registration names the same keys
/// [`probe_availability`] reports.
///
/// # Panics
///
/// Panics if `id` is not a valid Cosmos stable name.
#[must_use]
pub fn type_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new(id).expect("context type ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the provider key a coding topology's model access is declared under.
///
/// # Panics
///
/// Panics only if [`MODEL_PROVIDER`] stops being a valid stable name.
#[must_use]
pub fn model_provider_key() -> ProviderKey {
    ProviderKey::new(
        ProviderId::new(MODEL_PROVIDER).expect("provider ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the coding contract: produce one terminal assessment conclusion.
///
/// The signature names only [`AssessmentConclusion`], a Cosmos type. Nothing
/// coding-specific appears on the contract — the coding payload rides inside
/// the assessment's subject type parameter instead.
///
/// # Errors
///
/// Returns an error if the contract metadata is rejected.
pub fn contract() -> Result<TopologyContract, CatalogError> {
    TopologyContract::new(
        TopologyContractKey::new(
            TopologyContractId::new(CONTRACT).expect("contract ID is a valid stable name"),
            behavior_version(),
        ),
        GraphSignature::new().produce::<AssessmentConclusion>(),
        "resolve a request about the working tree and judge the outcome",
        "one immutable assessment conclusion over the subjects the request concerned",
    )
    .map_err(CatalogError::from)
}

/// Returns the configuration schema every coding registration resolves under.
#[must_use]
pub fn configuration_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("polaris-code.coding-configuration")
            .expect("configuration schema ID is a valid stable name"),
        behavior_version(),
    )
}

fn coding_type_explanations() -> Result<TypeExplanationRegistry, CatalogError> {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_resource::<CodingWorkspace>(type_key("polaris-code.coding-workspace"))
        .map_err(CatalogError::from)?;
    registry
        .bind_resource::<ProjectTestRunner>(type_key("polaris-code.project-test-runner"))
        .map_err(CatalogError::from)?;
    registry
        .bind_resource::<SweepWorklist>(type_key("polaris-code.sweep-worklist"))
        .map_err(CatalogError::from)?;
    registry
        .bind_resource::<ModelRegistry>(type_key(MODEL_REGISTRY_TYPE))
        .map_err(CatalogError::from)?;
    registry
        .bind_resource::<TokenMeter>(type_key(RUN_METER_TYPE))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<AssessmentConclusion>(type_key("polaris-code.assessment-conclusion"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<PredictedEdit>(type_key("polaris-code.predicted-edit"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<AppliedEdit>(type_key("polaris-code.applied-edit"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<VerifiedEdit>(type_key("polaris-code.verified-edit"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<EditFeedback>(type_key("polaris-code.edit-feedback"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<SweepProgress>(type_key("polaris-code.sweep-progress"))
        .map_err(CatalogError::from)?;

    let site_key = type_key("polaris-code.edit-site");
    registry
        .bind_output::<EditSite>(site_key.clone())
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Vec<EditSite>>(type_key("polaris-code.edit-sites"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<SiteObservation>(type_key("polaris-code.site-observation"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Assessment<EditSite>>(type_key("polaris-code.site-assessment"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Vec<Assessment<EditSite>>>(type_key("polaris-code.site-assessments"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<AssessmentSummary>(type_key("polaris-code.assessment-summary"))
        .map_err(CatalogError::from)?;

    // The answer shape's own boundaries. A graph-boundary type that is not
    // bound here is excluded with `type-explanation.missing`, so the second
    // assessment subject has to be as explainable as the first.
    registry
        .bind_resource::<CodingQuestion>(type_key("polaris-code.coding-question"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<QuestionFinding>(type_key("polaris-code.question-finding"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Vec<QuestionFinding>>(type_key("polaris-code.question-findings"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Assessment<QuestionFinding>>(type_key("polaris-code.finding-assessment"))
        .map_err(CatalogError::from)?;
    registry
        .bind_output::<Vec<Assessment<QuestionFinding>>>(type_key(
            "polaris-code.finding-assessments",
        ))
        .map_err(CatalogError::from)?;

    coverage_sweep::register_type_explanations::<EditSite>(&mut registry, &site_key)
        .map_err(|error| CatalogError::Cognition(error.to_string()))?;
    bounded_correction::register_type_explanations::<EditSite>(&mut registry, &site_key)
        .map_err(|error| CatalogError::Cognition(error.to_string()))?;
    Ok(registry)
}

/// What the host has actually wired for one coding run.
#[derive(Debug, Clone, Copy)]
pub struct HostCapabilities {
    /// Whether a project test runner is installed in the run context.
    pub test_runner: bool,
    /// Whether a language-model provider is configured.
    pub model_provider: bool,
}

impl HostCapabilities {
    /// Returns the capabilities of a hermetic run: neither a test runner nor a
    /// model provider.
    #[must_use]
    pub const fn hermetic() -> Self {
        Self {
            test_runner: false,
            model_provider: false,
        }
    }
}

/// Observes what this host can supply for the given coding task.
///
/// The workspace resource's state is a real observation: it is available for
/// writing only if the sandbox root is a writable directory. The test runner
/// and model provider are reported from [`HostCapabilities`] rather than by
/// probing `PATH` or the environment, because availability is a claim about
/// what the *run context* will contain, and a `cargo` binary the host never
/// installs as a resource is not available to a graph.
///
/// # Errors
///
/// Returns an error if an availability record or the snapshot is rejected.
///
/// # Panics
///
/// Panics only if the compile-time coding subject key cannot be composed into
/// the Cosmos coverage point's stable private-state keys.
pub fn probe_availability(
    task: &CodingTask,
    snapshot_id: AvailabilitySnapshotId,
    captured_at_ms: u64,
    host: HostCapabilities,
) -> Result<RuntimeAvailabilitySnapshot, ContractError> {
    probe_workspace_availability(task.working_dir(), snapshot_id, captured_at_ms, host)
}

/// Observes what this host can supply for a workspace root, with no task.
///
/// This is the whole of the probe; [`probe_availability`] is the task-shaped
/// call into it. Listing what a build can run has to be answerable before any
/// task exists, and it has to agree with what selection will decide, so both
/// go through this one implementation rather than through two that could
/// drift.
///
/// # Errors
///
/// Returns an error if an availability record or the snapshot is rejected.
///
/// # Panics
///
/// Panics only if the compile-time coding subject key cannot be composed into
/// the Cosmos coverage point's stable private-state keys.
pub fn probe_workspace_availability(
    workspace_root: &Path,
    snapshot_id: AvailabilitySnapshotId,
    captured_at_ms: u64,
    host: HostCapabilities,
) -> Result<RuntimeAvailabilitySnapshot, ContractError> {
    let observed = |state: AvailabilityState| {
        AvailabilityObservation::new(
            state,
            None,
            Some(DurationMs::new(60_000)),
            vec![provenance(
                SourceKind::HostObservation,
                EpistemicBasis::Verified,
            )],
        )
    };

    let writable = workspace_root
        .metadata()
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.permissions().readonly());
    let workspace_state = if writable {
        AvailabilityState::Available
    } else {
        AvailabilityState::Unavailable
    };

    let mut resources = vec![
        ResourceAvailability::new(
            type_key("polaris-code.coding-workspace"),
            ResourceAccess::Read,
            observed(workspace_state)?,
        )?,
        ResourceAvailability::new(
            type_key("polaris-code.sweep-worklist"),
            ResourceAccess::Write,
            observed(AvailabilityState::Available)?,
        )?,
        // Every topology run context gets its own token meter, so a graph that
        // bills its model spend can always find one.
        ResourceAvailability::new(
            type_key(RUN_METER_TYPE),
            ResourceAccess::Write,
            observed(AvailabilityState::Available)?,
        )?,
        // Always available, and deliberately so: the question is host-owned
        // data, not an installed capability, so the answer shape is
        // admissible wherever the working directory can be read.
        ResourceAvailability::new(
            type_key("polaris-code.coding-question"),
            ResourceAccess::Read,
            observed(workspace_state)?,
        )?,
    ];
    for state_key in coverage_sweep::state_resource_keys(&type_key("polaris-code.edit-site"))
        .expect("the coding edit-site key derives valid coverage state keys")
    {
        resources.push(ResourceAvailability::new(
            state_key,
            ResourceAccess::Write,
            observed(AvailabilityState::Available)?,
        )?);
    }
    if host.test_runner {
        resources.push(ResourceAvailability::new(
            type_key("polaris-code.project-test-runner"),
            ResourceAccess::Read,
            observed(AvailabilityState::Available)?,
        )?);
    }
    if host.model_provider {
        // The run context inherits the session's globals, so the registry the
        // host wired for the session is the registry a graph reads.
        resources.push(ResourceAvailability::new(
            type_key(MODEL_REGISTRY_TYPE),
            ResourceAccess::Read,
            observed(AvailabilityState::Available)?,
        )?);
    }

    // The loop and sweep hand-offs Polaris conservatively treats as free reads
    // are all produced inside their own graphs; the host declares them
    // available because it prepares the live context before readmission.
    let upstream_outputs = [
        "polaris-code.predicted-edit",
        "polaris-code.applied-edit",
        "polaris-code.verified-edit",
        "polaris-code.edit-feedback",
        "polaris-code.sweep-progress",
        "polaris-code.question-findings",
        "polaris-code.finding-assessments",
    ]
    .into_iter()
    .map(|id| OutputAvailability::new(type_key(id), observed(AvailabilityState::Available)?))
    .collect::<Result<Vec<_>, _>>()?;

    let providers = if host.model_provider {
        let provider = model_provider_key();
        vec![ProviderAvailability::new(
            provider.clone(),
            ContentDigest::sha256(MODEL_PROVIDER.as_bytes()),
            AvailabilityObservation::new(
                AvailabilityState::Available,
                Some(provider),
                Some(DurationMs::new(60_000)),
                vec![provenance(
                    SourceKind::HostObservation,
                    EpistemicBasis::Verified,
                )],
            )?,
        )?]
    } else {
        Vec::new()
    };

    RuntimeAvailabilitySnapshot::new(
        snapshot_id,
        EvidenceTime::new(None, Some(captured_at_ms))?,
        EnvironmentKey::new(
            EnvironmentId::new("polaris-code.local-sandbox")
                .expect("environment ID is a valid stable name"),
            behavior_version(),
        ),
        resources,
        upstream_outputs,
        providers,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        vec![provenance(
            SourceKind::HostObservation,
            EpistemicBasis::Verified,
        )],
    )
}

fn descriptor(
    id: &str,
    title: &str,
    summary: &str,
    contract: &TopologyContractKey,
) -> Result<TopologyDescriptor, CatalogError> {
    Ok(TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new(id).expect("topology ID is a valid stable name"),
            behavior_version(),
        ),
        title,
        summary,
    )?
    .claim(contract.clone()))
}

fn configuration(id: &str) -> Result<ResolvedConfigurationRef, ContractError> {
    ResolvedConfigurationRef::inline(
        configuration_schema(),
        ConfigurationValue::String(id.to_owned()),
    )
}

fn register_coding_topologies(catalog: &mut TopologyCatalog) -> Result<(), CatalogError> {
    let contract = contract()?;
    let key = contract.key().clone();
    catalog.register_contract(contract.clone())?;
    let explanations = coding_type_explanations()?;

    register_topology(
        catalog,
        &key,
        COVERAGE_AUDIT,
        "Coverage audit",
        "Cosmos coverage sweep embodied over real stub sites with its private boundaries explained by Cosmos.",
        graphs::coverage_audit_graph(),
        CandidateRequirements::none(),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        DIRECT,
        "Direct edit",
        "One read, one exact-string write, one verification; lowest latency and no second look.",
        graphs::direct_edit_graph(),
        CandidateRequirements::none(),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        MODEL_PROPOSED,
        "Model-proposed edit",
        "The direct shape with a language model proposing the change; needs a configured provider.",
        graphs::model_proposed_edit_graph(),
        CandidateRequirements::new(
            vec![ProviderRequirement::new(
                model_provider_key(),
                ContentDigest::sha256(MODEL_PROVIDER.as_bytes()),
            )],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        PREDICTION_ERROR,
        "Prediction-error edit",
        "Commits the expected post-edit file content, edits, then re-reads disk to falsify the prediction.",
        graphs::prediction_error_edit_graph(),
        CandidateRequirements::none(),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        TEST_GATED,
        "Test-gated edit",
        "Edits and gates the verdict on a real project test run; needs an installed test runner.",
        graphs::test_gated_edit_graph(),
        CandidateRequirements::none(),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        TREE_QUESTION,
        "Tree question",
        "Answers a bounded question about the working tree from the tree itself and stops; writes nothing.",
        graphs::tree_question_graph(),
        CandidateRequirements::none(),
        &explanations,
    )?;
    register_topology(
        catalog,
        &key,
        WORKLIST_SWEEP,
        "Worklist sweep edit",
        "Bounded embodiment-owned sweep over every real stub site, with unvisited sites kept visible.",
        graphs::worklist_sweep_graph(),
        CandidateRequirements::none(),
        &explanations,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "a complete pack registration keeps graph and all static metadata together"
)]
fn register_topology(
    catalog: &mut TopologyCatalog,
    contract_key: &TopologyContractKey,
    id: &str,
    title: &str,
    summary: &str,
    graph: Graph,
    requirements: CandidateRequirements,
    explanations: &TypeExplanationRegistry,
) -> Result<(), CatalogError> {
    let registration_id = RegistrationId::new(id)?;
    let descriptor = descriptor(id, title, summary, contract_key)?;
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(format!("polaris-code.revision:{id}").as_bytes()),
        configuration(id)?,
    )?;
    catalog.register_complete(CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new("polaris-code.coding-topology-pack")?,
        requirements,
        explanations.clone(),
        policy::author_priors(id)?,
    ))?;
    Ok(())
}

/// Why the coding catalog could not be assembled.
#[derive(Debug)]
#[non_exhaustive]
pub enum CatalogError {
    /// A Cosmos record rejected a reference or configuration.
    Contract(ContractError),
    /// The catalog rejected a registration.
    Catalog(TopologyCatalogError),
    /// Descriptor or contract metadata was rejected.
    Metadata(MetadataError),
    /// A stable name was rejected.
    Identifier(IdentifierError),
    /// Stable native-boundary explanations conflicted.
    Admission(AdmissionBuildError),
    /// A Cosmos cognition helper rejected a derived boundary identity.
    Cognition(String),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => error.fmt(formatter),
            Self::Catalog(error) => error.fmt(formatter),
            Self::Metadata(error) => write!(formatter, "invalid metadata field {}", error.field()),
            Self::Identifier(error) => error.fmt(formatter),
            Self::Admission(error) => error.fmt(formatter),
            Self::Cognition(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CatalogError {}

impl From<ContractError> for CatalogError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

impl From<TopologyCatalogError> for CatalogError {
    fn from(error: TopologyCatalogError) -> Self {
        Self::Catalog(error)
    }
}

impl From<MetadataError> for CatalogError {
    fn from(error: MetadataError) -> Self {
        Self::Metadata(error)
    }
}

impl From<IdentifierError> for CatalogError {
    fn from(error: IdentifierError) -> Self {
        Self::Identifier(error)
    }
}

impl From<AdmissionBuildError> for CatalogError {
    fn from(error: AdmissionBuildError) -> Self {
        Self::Admission(error)
    }
}

/// Returns the Cosmos topology keys this embodiment reuses unchanged.
///
/// Recorded so the consumer report can state exactly which shipped cognition
/// points were embodied rather than reimplemented.
#[must_use]
pub fn reused_cosmos_points() -> Vec<TopologyKey> {
    vec![
        prediction_error_loop::topology_key(),
        coverage_sweep::topology_key(),
    ]
}
