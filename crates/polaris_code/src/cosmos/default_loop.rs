//! Publication of the default conversational agent loop as a catalog
//! registration.
//!
//! Until this module existed the loop [`CodeAgent`] builds was the only way a
//! turn could be served, and it was invisible to the topology catalog: nothing
//! that enumerates registrations could name it, describe it, or answer whether
//! it was available. This pack publishes it — *as a registration, not as a
//! second implementation*. [`default_loop_graph`] calls
//! [`CodeAgent::build`](polaris::agent::Agent::build) and registers whatever it
//! produces, so the loop keeps exactly one definition.
//!
//! # Why this is a second pack and not another coding registration
//!
//! The coding contract ([`topology::contract`](super::topology::contract)) is
//! `produce::<AssessmentConclusion>()`. The loop's derived signature does not
//! satisfy it, on two independent axes:
//!
//! - `missing_produces`: the loop produces [`LlmResponse`], [`LoopState`] and
//!   [`PreparedContext`]; it never produces an `AssessmentConclusion`, because
//!   a conversational turn does not terminate in a judgement about a coding
//!   site.
//! - `extra_requires_outputs`: the loop's error handler reads [`CaughtError`]
//!   as a free upstream output, and the coding contract never sanctions that
//!   read.
//!
//! Neither can be repaired without restructuring the loop to produce a
//! conclusion, or widening a contract the coding registrations already
//! satisfy. `contract()` below is
//! therefore this pack's own contract, describing what a conversational turn
//! actually is. [`assert_default_loop_does_not_satisfy_the_coding_contract`]
//! keeps that finding executable rather than merely written down.
//!
//! # What the registration claims about itself
//!
//! Deliberately as little as possible, and nothing that is not observable:
//!
//! - **Priors: none.** The loop has no measured quality, coverage, latency or
//!   failure rate, and inventing a plausible set of numbers would make it win
//!   or lose selections on fiction. [`author_priors`] returns an empty vector,
//!   following the same precedent as the three prior-free coding registrations
//!   in [`policy`](super::policy).
//! - **Requirements: one provider.** A turn cannot be served without a
//!   configured model provider, so that is declared and mechanically checked.
//!   Everything else the loop reads is a session resource the host installs.
//! - **Availability: answered from the host, both ways.**
//!   [`probe_availability`] reports `Available` only for what
//!   [`ConversationalHost`] says is wired; a host without a model provider
//!   yields `provider.unavailable`, so the answer has a real negative case.
//!
//! # Publication is not activation
//!
//! Registering the loop changes which path serves a turn for no input. The
//! selection point in [`run`](crate::run) asks for the *coding* contract, under
//! which this registration is excluded with `contract.not-requested`; and the
//! conversational path in [`run`](crate::run) never consults the catalog at
//! all. Both facts are asserted in `tests/default_loop_publication.rs`.
//!
//! The full report — the diagnosis, the claims, and the remaining findings —
//! is in `docs/default-loop-publication.md`.

use polaris::agent::Agent;
use polaris::graph::{CaughtError, Graph, GraphSignature, RunCancellation};
use polaris::models::ModelRegistry;
use polaris::models::llm::LlmResponse;
use polaris::plugins::UserIO;
use polaris::sessions::SessionInfo;
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};
use polaris::tools::ToolRegistry;
use polaris_cosmos::contracts::{
    AvailabilityObservation, AvailabilitySnapshotId, AvailabilityState, ConfigurationSchemaId,
    ConfigurationSchemaKey, ConfigurationValue, ContentDigest, ContextTypeId, ContextTypeKey,
    ContractError, DurationMs, EnvironmentId, EnvironmentKey, EpistemicBasis, EvidenceTime,
    OutputAvailability, ProviderAvailability, ProviderId, ProviderKey, RegistrationSelectionRef,
    ResolvedConfigurationRef, ResourceAccess, ResourceAvailability, RuntimeAvailabilitySnapshot,
    ScoreComponent, SourceKind,
};
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, ProviderRequirement, RegistrationId,
    RegistrationProvenance, TopologyCatalog, TopologyContract, TopologyContractId,
    TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey, TypeExplanationRegistry,
};
use polaris_ontrace::oxigraph::OxigraphStore;

use crate::agents::auto_review::{ReviewHistory, ReviewIntent, ReviewPolicy, ReviewResult};
use crate::agents::code_agent::{PreparedContext, SecretRedactor};
use crate::agents::context::CompactionRun;
use crate::agents::permission_rules::{
    PermissionRules, RunToolGrants, SessionGrants, ShellApproval,
};
use crate::agents::retry::{AttemptLedger, AttemptOutcome, RetryStep};
use crate::agents::{
    AgentConfig, AgentLlm, CodeAgent, ContextManager, LoopState, PolicySnapshot, ProjectContext,
};
use crate::terminal::attachments::AttachmentStore;
use crate::terminal::background::BackgroundTasks;
use crate::terminal::transcript::LiveTranscript;
use crate::terminal::{SessionSettings, TurnSteering};
use crate::tools::ToolPolicyRegistry;
use crate::usage::TokenMeter;

use super::topology::{CatalogError, MODEL_PROVIDER, MODEL_REGISTRY_TYPE, RUN_METER_TYPE};
use super::{behavior_version, provenance};

/// Registration identity of the default conversational agent loop.
pub const DEFAULT_LOOP: &str = "code.default-agent-loop";

/// Stable identity of the contract a conversational turn is served under.
pub const CONVERSATION_CONTRACT: &str = "polaris-code.conversational-turn";

/// Stable name this pack attributes its registration to.
pub const PACK_PROVIDER: &str = "polaris-code.default-loop-pack";

/// Contributes the default conversational agent loop to the process-wide
/// topology catalog under its own contract.
///
/// Add this plugin when a host should be able to *see* the path that serves
/// ordinary turns. It does not change which path serves them: nothing in
/// [`run`](crate::run) asks for [`CONVERSATION_CONTRACT`], and the
/// conversational turn never consults the catalog.
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
/// During `build`, the pack registers [`CONVERSATION_CONTRACT`] and one
/// [`CompleteTopologyRegistration`] whose graph is built by
/// [`CodeAgent`]. The provider validates and seals the combined catalog during
/// `ready`.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_code::cosmos::default_loop::DefaultLoopTopologyPackPlugin;
/// use polaris_cosmos::topology::TopologyCatalogPlugin;
///
/// # async fn assemble() {
/// let mut server = Server::new();
/// server
///     .add_plugins(TopologyCatalogPlugin)
///     .add_plugins(DefaultLoopTopologyPackPlugin);
/// server.finish().await.expect("default loop pack should assemble");
/// # }
/// ```
#[derive(Debug, Default, Copy, Clone)]
pub struct DefaultLoopTopologyPackPlugin;

#[plugin(id = "polaris_code::default_loop_topology_pack", version = "0.1.0")]
impl Plugin for DefaultLoopTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        if let Err(error) = register_default_loop(&mut catalog) {
            panic!("default loop topology pack registration failed: {error}");
        }
    }
}

fn type_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new(id).expect("context type ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the provider key a conversational turn needs configured.
#[must_use]
pub fn model_provider_key() -> ProviderKey {
    ProviderKey::new(
        ProviderId::new(MODEL_PROVIDER).expect("provider ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the key of the contract a conversational turn is served under.
#[must_use]
pub fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new(CONVERSATION_CONTRACT).expect("contract ID is a valid stable name"),
        behavior_version(),
    )
}

/// Returns the conversational-turn contract.
///
/// The signature is the smallest honest statement of what serving a turn is:
/// read the user I/O boundary, drive the turn's loop state, and produce a model
/// response. [`require_output::<CaughtError>`](GraphSignature::require_output)
/// is what sanctions an error handler — without it no graph that recovers from
/// a caught error could ever claim this contract, because `satisfies` refuses
/// free output reads the slot did not name.
///
/// # Errors
///
/// Returns an error if the contract metadata is rejected.
pub fn contract() -> Result<TopologyContract, CatalogError> {
    TopologyContract::new(
        contract_key(),
        GraphSignature::new()
            .require_read::<UserIO>()
            .require_write::<LoopState>()
            .require_output::<CaughtError>()
            .produce::<LlmResponse>(),
        "serve one conversational turn for the user",
        "one model response for the turn, with the loop state left settled",
    )
    .map_err(CatalogError::from)
}

/// Returns the configuration schema the conversational registration resolves
/// under.
#[must_use]
pub fn configuration_schema() -> ConfigurationSchemaKey {
    ConfigurationSchemaKey::new(
        ConfigurationSchemaId::new("polaris-code.conversational-configuration")
            .expect("configuration schema ID is a valid stable name"),
        behavior_version(),
    )
}

/// Builds the default agent loop's graph.
///
/// This is the whole point of the module: the graph is produced by
/// [`CodeAgent::build`](polaris::agent::Agent::build), the same call the
/// [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin) makes when it registers
/// the agent for live turns. There is no second wiring of the loop here to
/// drift from it.
#[must_use]
pub fn default_loop_graph() -> Graph {
    let mut graph = Graph::new();
    CodeAgent.build(&mut graph);
    graph
}

/// Binds every boundary the default loop touches to a stable key.
///
/// Mechanical admission projects the derived signature through this registry;
/// a boundary missing here is excluded with `type-explanation.missing` rather
/// than silently ignored, so this list must stay exhaustive as the loop's
/// systems change.
///
/// # Errors
///
/// Returns an error if two boundaries collide on one stable key.
pub fn type_explanations() -> Result<TypeExplanationRegistry, CatalogError> {
    let mut registry = TypeExplanationRegistry::new();

    // Resources the loop's systems read or write. The resource and output
    // channels are separate bijections, so `LoopState` may carry one identity
    // in both.
    registry.bind_resource::<ProjectContext>(type_key("polaris-code.project-context"))?;
    registry.bind_resource::<SessionInfo>(type_key("polaris-code.session-info"))?;
    registry.bind_resource::<LoopState>(type_key("polaris-code.agent-loop-state"))?;
    registry.bind_resource::<UserIO>(type_key("polaris-code.user-io"))?;
    registry.bind_resource::<AttachmentStore>(type_key("polaris-code.attachment-store"))?;
    registry.bind_resource::<SessionSettings>(type_key("polaris-code.session-settings"))?;
    registry.bind_resource::<ContextManager>(type_key("polaris-code.context-manager"))?;
    registry.bind_resource::<AgentLlm>(type_key("polaris-code.agent-llm"))?;
    registry.bind_resource::<AgentConfig>(type_key("polaris-code.agent-config"))?;
    registry.bind_resource::<BackgroundTasks>(type_key("polaris-code.background-tasks"))?;
    registry.bind_resource::<OxigraphStore>(type_key("polaris-code.oxigraph-store"))?;
    // A stable key names one native type across the whole vocabulary, so both
    // packs must spell these two the same way. The coding pack already owns
    // the spelling; this pack reuses its constants rather than minting a
    // second key for the same Rust type.
    registry.bind_resource::<ModelRegistry>(type_key(MODEL_REGISTRY_TYPE))?;
    registry.bind_resource::<TokenMeter>(type_key(RUN_METER_TYPE))?;
    registry.bind_resource::<LiveTranscript>(type_key("polaris-code.live-transcript"))?;
    registry.bind_resource::<PolicySnapshot>(type_key("polaris-code.policy-snapshot"))?;
    registry.bind_resource::<TurnSteering>(type_key("polaris-code.turn-steering"))?;
    registry.bind_resource::<RunCancellation>(type_key("polaris-code.run-cancellation"))?;
    registry.bind_resource::<ToolRegistry>(type_key("polaris-code.tool-registry"))?;
    registry.bind_resource::<SecretRedactor>(type_key("polaris-code.secret-redactor"))?;
    registry.bind_resource::<AttemptLedger>(type_key("polaris-code.attempt-ledger"))?;

    registry.bind_resource::<PermissionRules>(type_key("polaris-code.permission-rules"))?;
    registry.bind_resource::<SessionGrants>(type_key("polaris-code.session-grants"))?;
    registry.bind_resource::<ShellApproval>(type_key("polaris-code.shell-approval"))?;
    registry.bind_resource::<RunToolGrants>(type_key("polaris-code.run-tool-grants"))?;
    registry.bind_resource::<ToolPolicyRegistry>(type_key("polaris-code.tool-policy-registry"))?;

    registry.bind_resource::<ReviewPolicy>(type_key("polaris-code.review-policy"))?;
    registry.bind_resource::<ReviewIntent>(type_key("polaris-code.review-intent"))?;
    registry.bind_resource::<ReviewHistory>(type_key("polaris-code.review-history"))?;
    registry.bind_output::<ReviewResult>(type_key("polaris-code.review-result"))?;

    // Outputs the loop produces, plus the one free output its error handler
    // reads.
    registry.bind_output::<LlmResponse>(type_key("polaris-code.llm-response"))?;
    registry.bind_output::<LoopState>(type_key("polaris-code.agent-loop-state"))?;
    registry.bind_output::<PreparedContext>(type_key("polaris-code.prepared-context"))?;
    registry.bind_output::<CompactionRun>(type_key("polaris-code.compaction-run"))?;
    registry.bind_output::<AttemptOutcome>(type_key("polaris-code.attempt-outcome"))?;
    registry.bind_output::<RetryStep>(type_key("polaris-code.retry-step"))?;
    registry.bind_output::<CaughtError>(type_key("polaris-code.caught-error"))?;

    Ok(registry)
}

/// The declared requirements of a conversational turn.
///
/// One provider, and nothing else. The loop's resource requirements are not
/// repeated here: admission derives them from the graph's own signature, so
/// restating them would be a second place to keep in step with the loop.
#[must_use]
pub fn requirements() -> CandidateRequirements {
    CandidateRequirements::new(
        vec![ProviderRequirement::new(
            model_provider_key(),
            ContentDigest::sha256(MODEL_PROVIDER.as_bytes()),
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Returns the immutable author priors stored with this registration: none.
///
/// The default loop has no measured quality, verified coverage, latency, or
/// failure rate — it is the path everything fell back to, which is exactly why
/// nothing was ever measured about it. Asserting numbers anyway would let it
/// win or lose selections on invented evidence. Carrying no priors makes it
/// discoverable and describable without making it preferable, and any host
/// that later wants to rank it must first produce facts about it.
///
/// This is also the honest limit of what publishing the loop establishes: selection
/// cannot currently be *grounded* in facts about the default loop, only kept
/// free of fictional ones. See `docs/default-loop-publication.md`.
#[must_use]
pub fn author_priors() -> Vec<ScoreComponent> {
    Vec::new()
}

/// What the host has actually wired for one conversational turn.
#[derive(Debug, Clone, Copy)]
pub struct ConversationalHost {
    /// Whether the session-scoped agent resources are installed in the run
    /// context.
    pub session_resources: bool,
    /// Whether a language-model provider is configured.
    pub model_provider: bool,
}

impl ConversationalHost {
    /// Returns the capabilities of a hermetic run: no session, no provider.
    #[must_use]
    pub const fn hermetic() -> Self {
        Self {
            session_resources: false,
            model_provider: false,
        }
    }

    /// Returns the capabilities of the assembled binary serving a live turn.
    #[must_use]
    pub const fn live() -> Self {
        Self {
            session_resources: true,
            model_provider: true,
        }
    }
}

/// Every resource the default loop reads or writes, with the access it needs.
///
/// Write access subsumes read at the admission boundary, so the five resources
/// the loop both reads and writes appear once, as writes.
///
/// One of them, the secret redactor, is process configuration rather than
/// session state. It is still reported from [`ConversationalHost`] with the
/// rest: a plugin has to install it before the loop can read it, which is the
/// same claim the other entries make.
const LOOP_RESOURCES: [(&str, ResourceAccess); 28] = [
    ("polaris-code.review-policy", ResourceAccess::Read),
    ("polaris-code.review-intent", ResourceAccess::Write),
    ("polaris-code.review-history", ResourceAccess::Write),
    ("polaris-code.agent-config", ResourceAccess::Read),
    ("polaris-code.agent-llm", ResourceAccess::Write),
    ("polaris-code.attempt-ledger", ResourceAccess::Write),
    ("polaris-code.agent-loop-state", ResourceAccess::Write),
    ("polaris-code.attachment-store", ResourceAccess::Read),
    ("polaris-code.background-tasks", ResourceAccess::Read),
    ("polaris-code.context-manager", ResourceAccess::Write),
    ("polaris-code.live-transcript", ResourceAccess::Write),
    ("polaris-code.model-registry", ResourceAccess::Read),
    ("polaris-code.oxigraph-store", ResourceAccess::Read),
    ("polaris-code.policy-snapshot", ResourceAccess::Write),
    // Written as well as read: `refresh_project_docs` re-reads the project
    // instruction chain at the turn boundary and replaces the documents.
    ("polaris-code.project-context", ResourceAccess::Write),
    ("polaris-code.run-cancellation", ResourceAccess::Read),
    ("polaris-code.run-token-meter", ResourceAccess::Read),
    ("polaris-code.secret-redactor", ResourceAccess::Read),
    ("polaris-code.permission-rules", ResourceAccess::Read),
    ("polaris-code.session-grants", ResourceAccess::Read),
    ("polaris-code.shell-approval", ResourceAccess::Read),
    ("polaris-code.run-tool-grants", ResourceAccess::Read),
    ("polaris-code.tool-policy-registry", ResourceAccess::Read),
    ("polaris-code.session-info", ResourceAccess::Read),
    ("polaris-code.session-settings", ResourceAccess::Read),
    ("polaris-code.tool-registry", ResourceAccess::Read),
    ("polaris-code.turn-steering", ResourceAccess::Read),
    ("polaris-code.user-io", ResourceAccess::Read),
];

/// Observes whether this host can serve a conversational turn.
///
/// The session resources are reported from [`ConversationalHost`] rather than
/// probed, for the same reason the coding pack reports its test runner that
/// way: availability is a claim about what the *run context* will contain, and
/// a type that exists in the binary is not a resource a graph can read unless a
/// plugin installed it.
///
/// # Errors
///
/// Returns an error if an availability record or the snapshot is rejected.
pub fn probe_availability(
    snapshot_id: AvailabilitySnapshotId,
    captured_at_ms: u64,
    host: ConversationalHost,
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
    let session_state = if host.session_resources {
        AvailabilityState::Available
    } else {
        AvailabilityState::Unavailable
    };

    let resources = LOOP_RESOURCES
        .into_iter()
        .map(|(id, access)| {
            ResourceAvailability::new(type_key(id), access, observed(session_state)?)
        })
        .collect::<Result<Vec<_>, _>>()?;

    // The caught error the loop's handler reads is produced inside the loop by
    // the executor's error path, exactly like the coding pack's intra-graph
    // hand-offs: Polaris derives it conservatively as a free read, and the host
    // reports it available because the run context that carries it is the same
    // execution.
    let upstream_outputs = vec![OutputAvailability::new(
        type_key("polaris-code.caught-error"),
        observed(session_state)?,
    )?];

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
            EnvironmentId::new("polaris-code.terminal-session")
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

/// Registers the conversational contract and the default loop against it.
///
/// # Errors
///
/// Returns an error if the contract, the metadata, the type explanations, or
/// the registration are rejected by the catalog.
pub fn register_default_loop(catalog: &mut TopologyCatalog) -> Result<(), CatalogError> {
    let contract = contract()?;
    let key = contract.key().clone();
    catalog.register_contract(contract)?;

    let registration_id = RegistrationId::new(DEFAULT_LOOP)?;
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new(DEFAULT_LOOP).expect("topology ID is a valid stable name"),
            behavior_version(),
        ),
        "Default agent loop",
        "The ordinary conversational turn: steer, project context, act, run tools or finalize, recover on error.",
    )?
    .claim(key);
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(format!("polaris-code.revision:{DEFAULT_LOOP}").as_bytes()),
        ResolvedConfigurationRef::inline(
            configuration_schema(),
            ConfigurationValue::String(DEFAULT_LOOP.to_owned()),
        )?,
    )?;

    catalog.register_complete(CompleteTopologyRegistration::new(
        selection,
        descriptor,
        default_loop_graph(),
        RegistrationProvenance::new(PACK_PROVIDER)?,
        requirements(),
        type_explanations()?,
        author_priors(),
    ))?;
    Ok(())
}

/// Asserts the recorded finding: the default loop cannot claim the coding
/// contract, and names exactly what fails.
///
/// Exposed rather than left in a test file because it is the answer to
/// "does the existing contract admit it?", and that answer has to stay true as
/// the loop and the coding contract each evolve. Returns the two violating
/// axes as human-readable type names: the coding outputs the loop does not
/// produce, and the free output reads the coding contract never sanctioned.
///
/// # Panics
///
/// Panics if the coding contract cannot be constructed, which its own
/// constants make impossible.
#[must_use]
pub fn assert_default_loop_does_not_satisfy_the_coding_contract() -> (Vec<String>, Vec<String>) {
    let coding = super::topology::contract().expect("the coding contract is well formed");
    let diff = default_loop_graph()
        .signature()
        .satisfies_diff(coding.signature());
    (
        diff.missing_produces()
            .iter()
            .map(|access| access.type_name.to_owned())
            .collect(),
        diff.extra_requires_outputs()
            .iter()
            .map(|access| access.type_name.to_owned())
            .collect(),
    )
}
