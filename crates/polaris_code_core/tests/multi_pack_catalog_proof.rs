//! Cross-pack consumer proof: two independent packs, one sealed catalog.
//!
//! `tests/cosmos_contracts_proof.rs` proves one pack. This file proves the
//! property that makes packs pluggable: a **second**
//! independent pack, owned by nobody the host selector knows about, joins the
//! same sealed [`TopologyCatalog`] and is discovered, preflighted, and ranked
//! by exactly the same host code.
//!
//! The review pack below is a complete stranger to `polaris_code_core`: its own
//! contract, its own graphs, its own assessment subject, its own boundary types,
//! its own configuration schema, its own provenance, and its own author priors.
//! Nothing in `src/` mentions it. The only host code that touches it is
//! [`polaris_code_core::cosmos::policy::candidate_facts`] and the generic
//! selection driver in this file — neither of which can name a pack.
//!
//! What each test pins:
//!
//! | Test | Property |
//! |---|---|
//! | `two_independent_packs_appear_in_one_sealed_snapshot` | Both packs' registrations are in one catalog, all complete. |
//! | `pack_insertion_order_does_not_change_the_sealed_catalog` | Sealed contents, digest, and generation are order-free. |
//! | `pack_insertion_order_does_not_change_selection_outcomes` | Every decision fingerprint is order-free. |
//! | `one_missing_type_explanation_stops_assembly_before_preflight` | A malformed registration is a typed catalog error, and the catalog is unchanged. |
//! | `a_malformed_pack_fails_server_assembly_rather_than_sealing` | The provider never seals; the failure is not a runtime exclusion. |
//! | `the_selector_reads_both_packs_through_the_catalog_alone` | Every selector input for both packs comes from `Res<TopologyCatalog>`. |
//! | `one_generic_selection_driver_serves_both_packs` | The same driver, given only a contract key, ranks each pack. |
//! | `two_packs_must_agree_on_the_stable_key_of_a_shared_native_type` | The recorded consumer seam, at assembly. |
//! | `the_conflicting_binding_is_a_typed_catalog_error` | The same seam, as an inspectable typed error. |
//! | `registration_order_inside_one_pack_is_also_irrelevant` | Order-freedom also holds below the plugin boundary. |
//! | `no_production_source_mentions_the_second_pack` | Nothing under `src/` can name this pack. |
//! | `sibling_pack_build_order_is_not_the_host_add_plugins_order` | The order tests above are not vacuous, and a recorded seam. |

use std::collections::BTreeMap;

use polaris::graph::{Graph, GraphSignature};
use polaris::system::param::Res;
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin, PluginId, Version};
use polaris::system::resource::LocalResource;
use polaris::system::server::Server;
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::{CodingIntent, CodingObjective, CodingTask};
use polaris_code_core::cosmos::topology::{
    self, CodingTopologyPackPlugin, HostCapabilities, probe_availability,
};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, AssessmentSummary, AvailabilitySnapshotId, ConfidencePpm,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    ContextTypeId, ContextTypeKey, ContractError, CoveragePpm, DurationMs, EpistemicBasis,
    EvidenceKind, EvidenceObjectId, EvidenceRef, EvidenceStoreId, Measurement, MeasurementValue,
    PolicyId, PolicyRef, PreferenceCriterion, Provenance, ProviderId, ProviderKey,
    RegistrationSelectionRef, ResolvedConfigurationRef, RuntimeAvailabilitySnapshot,
    ScoreComponent, ScoreComponentId, ScoreSource, SelectionDecision, SelectionDecisionId,
    SelectionOutcome, SourceId, SourceKey, SourceKind, Status, StopReason, SubjectSchema,
    SubjectSchemaId, SubjectSchemaKey, TaskProfile, UtilityPpm,
};
use polaris_cosmos::evidence::{BoundedInMemoryEvidence, TopologyEvidence, TopologyStatsSnapshot};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    CandidateAdmissionSet, CandidateRequirements, CompleteTopologyRegistration,
    ProviderRequirement, RegistrationId, RegistrationProvenance, TopologyCatalog,
    TopologyCatalogError, TopologyCatalogPlugin, TopologyCatalogSnapshot, TopologyContract,
    TopologyContractId, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
    TypeExplanationRegistry, preflight_catalog,
};

// ===========================================================================
// The second pack: a change-review pack that `polaris_code` does not know.
// ===========================================================================

/// Contract the review pack owns. It is never the coding contract.
const REVIEW_CONTRACT: &str = "polaris-review.change-assessment";

/// Fast single-pass review registration.
const SINGLE_PASS: &str = "review.single-pass";

/// Slow, high-coverage review registration.
const THOROUGH_PASS: &str = "review.thorough-pass";

/// Registration whose graph reads a corpus resource this host never wires.
const CORPUS_SCAN: &str = "review.corpus-scan";

/// Registration that declares a grading provider this host never wires.
const MODEL_GRADED: &str = "review.model-graded";

/// Registration deliberately left with one unexplained boundary.
const UNEXPLAINED: &str = "review.unexplained";

/// Grading provider the review pack declares and this host does not supply.
const REVIEW_ORACLE: &str = "polaris-review.grading-oracle";

/// The review pack's own provenance, distinct from the coding pack's.
const REVIEW_PROVIDER: &str = "polaris-review.change-review-pack";

/// The review pack's own evidence store.
const REVIEW_EVIDENCE_STORE: &str = "polaris-review.review-evidence";

/// One reviewed change: the review pack's own assessment subject.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ChangeNote {
    path: String,
    concern: String,
}

/// Corpus the corpus-scan graph reads. Never wired by the coding host.
#[derive(Debug)]
struct ReviewCorpus;

impl LocalResource for ReviewCorpus {}

/// Resource the malformed registration reads without explaining it.
#[derive(Debug)]
struct UnexplainedCorpus;

impl LocalResource for UnexplainedCorpus {}

fn review_version() -> Version {
    Version::new(1, 0, 0)
}

fn review_type_key(id: &str) -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new(id).expect("review type ID is a valid stable name"),
        review_version(),
    )
}

/// The stable output key the coding pack already bound `AssessmentConclusion` to.
///
/// This literal is copied out of `src/cosmos/topology.rs`, where it is a private
/// string. It has to be: the type-explanation registry is one process-global
/// bijection, so the second pack to bind the *same* native type under a
/// *different* stable key fails the whole assembly. `AssessmentConclusion` is a
/// Cosmos type that every contract in this catalog terminates in, and Cosmos
/// ships no canonical key for it — so the first pack to bind it silently fixes
/// the key for every later pack. Recorded as a consumer seam and pinned by
/// `two_packs_must_agree_on_the_stable_key_of_a_shared_native_type`.
fn coding_owned_conclusion_key() -> ContextTypeKey {
    ContextTypeKey::new(
        ContextTypeId::new("polaris-code.assessment-conclusion")
            .expect("the coding conclusion key is a valid stable name"),
        Version::new(1, 0, 0),
    )
}

fn review_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new(REVIEW_CONTRACT).expect("review contract ID is valid"),
        review_version(),
    )
}

fn coding_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new(topology::CONTRACT).expect("coding contract ID is valid"),
        Version::new(1, 0, 0),
    )
}

fn review_provenance(kind: SourceKind, basis: EpistemicBasis) -> Provenance {
    Provenance::new(
        SourceKey::new(
            SourceId::new(REVIEW_PROVIDER).expect("review source ID is valid"),
            review_version(),
        ),
        kind,
        basis,
    )
}

fn review_policy(id: &str) -> PolicyRef {
    PolicyRef::new(
        PolicyId::new(id).expect("review policy ID is valid"),
        review_version(),
    )
}

fn review_subject_schema() -> SubjectSchema<ChangeNote> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("polaris-review.change-note").expect("subject schema ID is valid"),
            review_version(),
        ),
        "One reviewed change: a path and the concern that was checked.",
        r#"{"type":"object","required":["path","concern"]}"#,
        |subject: &ChangeNote| {
            if subject.path.is_empty() {
                Err("a change note must name a path".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .expect("review subject schema is valid")
}

/// Builds the review pack's terminal conclusion from one reviewed note.
fn review_conclusion(concern: &str) -> Result<AssessmentConclusion, ContractError> {
    let assessment = Assessment::new(
        &review_subject_schema(),
        ChangeNote {
            path: "src/lib.rs".to_owned(),
            concern: concern.to_owned(),
        },
        Status::Pass,
        vec![
            review_provenance(SourceKind::AutomatedVerifier, EpistemicBasis::Verified)
                .with_evidence(EvidenceRef::new(
                    EvidenceStoreId::new(REVIEW_EVIDENCE_STORE).expect("evidence store is valid"),
                    EvidenceKind::VerifierOutput,
                    EvidenceObjectId::new("review.change-note").expect("evidence object is valid"),
                )),
        ],
    )?;
    let assessments = [assessment];
    let summary = AssessmentSummary::new(
        &assessments,
        review_policy("polaris-review.combine-notes"),
        ContentDigest::sha256(b"polaris-review.combine-notes"),
        vec![review_provenance(
            SourceKind::CosmosPolicy,
            EpistemicBasis::ReDerived,
        )],
    )?;
    AssessmentConclusion::new(summary, Vec::new(), StopReason::GoalMet, &assessments)
}

fn conclusion_or_error(concern: &str) -> Result<AssessmentConclusion, SystemError> {
    review_conclusion(concern).map_err(|error| SystemError::ExecutionError(error.to_string()))
}

#[system]
async fn single_pass_review() -> Result<AssessmentConclusion, SystemError> {
    conclusion_or_error("single-pass")
}

#[system]
async fn thorough_review() -> Result<AssessmentConclusion, SystemError> {
    conclusion_or_error("thorough")
}

#[system]
async fn corpus_review(_corpus: Res<ReviewCorpus>) -> Result<AssessmentConclusion, SystemError> {
    conclusion_or_error("corpus")
}

#[system]
async fn model_graded_review() -> Result<AssessmentConclusion, SystemError> {
    conclusion_or_error("model-graded")
}

#[system]
async fn unexplained_review(
    _corpus: Res<UnexplainedCorpus>,
) -> Result<AssessmentConclusion, SystemError> {
    conclusion_or_error("unexplained")
}

/// The review pack's type explanations.
///
/// `UnexplainedCorpus` is deliberately absent: it is the boundary the malformed
/// registration forgets, and the registry is process-global once merged, so
/// binding it here would hide the very failure this proof pins.
fn review_type_explanations() -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_output::<AssessmentConclusion>(coding_owned_conclusion_key())
        .expect("the review pack accepts the process-wide conclusion key");
    registry
        .bind_resource::<ReviewCorpus>(review_type_key("polaris-review.corpus"))
        .expect("the review corpus binds once");
    registry
}

struct ReviewPrior {
    quality: u32,
    coverage: u32,
    latency_ms: u64,
    latency_utility: u32,
    failure_probability: u32,
    failure_utility: u32,
}

fn review_prior(registration: &str) -> Option<ReviewPrior> {
    match registration {
        SINGLE_PASS => Some(ReviewPrior {
            quality: 600_000,
            coverage: 200_000,
            latency_ms: 30,
            latency_utility: 950_000,
            failure_probability: 300_000,
            failure_utility: 700_000,
        }),
        THOROUGH_PASS => Some(ReviewPrior {
            quality: 900_000,
            coverage: 900_000,
            latency_ms: 400,
            latency_utility: 350_000,
            failure_probability: 50_000,
            failure_utility: 950_000,
        }),
        _ => None,
    }
}

fn review_component(
    registration: &str,
    suffix: &str,
    criterion: PreferenceCriterion,
    value: MeasurementValue,
    utility: u32,
) -> Result<ScoreComponent, ContractError> {
    let confidence = ConfidencePpm::new(650_000)?;
    ScoreComponent::new(
        ScoreComponentId::new(format!("{registration}.{suffix}"))?,
        criterion,
        ScoreSource::AuthorPrior,
        Measurement::new(
            value,
            None,
            confidence,
            vec![review_provenance(
                SourceKind::PolicyDefault,
                EpistemicBasis::Asserted,
            )],
        )?,
        review_policy("polaris-review.linear-utility"),
        UtilityPpm::new(utility)?,
        confidence,
        vec![review_provenance(
            SourceKind::PolicyDefault,
            EpistemicBasis::Asserted,
        )],
        None,
    )
}

fn review_author_priors(registration: &str) -> Result<Vec<ScoreComponent>, ContractError> {
    let Some(prior) = review_prior(registration) else {
        return Ok(Vec::new());
    };
    Ok(vec![
        review_component(
            registration,
            "quality",
            PreferenceCriterion::Quality,
            MeasurementValue::Proportion(CoveragePpm::new(prior.quality)?),
            prior.quality,
        )?,
        review_component(
            registration,
            "coverage",
            PreferenceCriterion::VerifiedCoverage,
            MeasurementValue::Proportion(CoveragePpm::new(prior.coverage)?),
            prior.coverage,
        )?,
        review_component(
            registration,
            "latency",
            PreferenceCriterion::Latency,
            MeasurementValue::Duration(DurationMs::new(prior.latency_ms)),
            prior.latency_utility,
        )?,
        review_component(
            registration,
            "failure-risk",
            PreferenceCriterion::ExecutionFailureRisk,
            MeasurementValue::Proportion(CoveragePpm::new(prior.failure_probability)?),
            prior.failure_utility,
        )?,
    ])
}

fn review_configuration(id: &str) -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("polaris-review.review-configuration")
                .expect("review configuration schema ID is valid"),
            review_version(),
        ),
        ConfigurationValue::String(id.to_owned()),
    )
    .expect("review configuration is valid")
}

fn review_registration(
    id: &str,
    title: &str,
    summary: &str,
    graph: Graph,
    requirements: CandidateRequirements,
    explanations: TypeExplanationRegistry,
) -> CompleteTopologyRegistration {
    let registration_id = RegistrationId::new(id).expect("review registration ID is valid");
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new(id).expect("review topology ID is valid"),
            review_version(),
        ),
        title,
        summary,
    )
    .expect("review descriptor metadata is valid")
    .claim(review_contract_key());
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(format!("polaris-review.revision:{id}").as_bytes()),
        review_configuration(id),
    )
    .expect("review selection metadata is valid");
    CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new(REVIEW_PROVIDER).expect("review provider ID is valid"),
        requirements,
        explanations,
        review_author_priors(id).expect("review author priors are valid"),
    )
}

fn review_oracle_requirement() -> CandidateRequirements {
    CandidateRequirements::new(
        vec![ProviderRequirement::new(
            ProviderKey::new(
                ProviderId::new(REVIEW_ORACLE).expect("review provider ID is valid"),
                review_version(),
            ),
            ContentDigest::sha256(REVIEW_ORACLE.as_bytes()),
        )],
        Vec::new(),
        Vec::new(),
        Vec::new(),
    )
}

/// Registers the review pack's own semantic contract.
fn register_review_contract(catalog: &mut TopologyCatalog) -> Result<(), TopologyCatalogError> {
    catalog.register_contract(
        TopologyContract::new(
            review_contract_key(),
            GraphSignature::new().produce::<AssessmentConclusion>(),
            "review one proposed change and judge whether it is safe to land",
            "one immutable assessment conclusion over the reviewed change notes",
        )
        .expect("review contract metadata is valid"),
    )
}

/// Builds the review pack's four registrations in canonical authoring order.
fn review_registrations() -> Vec<CompleteTopologyRegistration> {
    let mut single = Graph::new();
    single.add_system(single_pass_review);
    let mut thorough = Graph::new();
    thorough.add_system(thorough_review);
    let mut corpus = Graph::new();
    corpus.add_system(corpus_review);
    let mut graded = Graph::new();
    graded.add_system(model_graded_review);
    vec![
        review_registration(
            SINGLE_PASS,
            "Single-pass review",
            "One read and one verdict; lowest latency and no second opinion.",
            single,
            CandidateRequirements::none(),
            review_type_explanations(),
        ),
        review_registration(
            THOROUGH_PASS,
            "Thorough review",
            "Re-reads every touched region before concluding; highest coverage, slowest.",
            thorough,
            CandidateRequirements::none(),
            review_type_explanations(),
        ),
        review_registration(
            CORPUS_SCAN,
            "Corpus-backed review",
            "Compares the change against an indexed review corpus resource.",
            corpus,
            CandidateRequirements::none(),
            review_type_explanations(),
        ),
        review_registration(
            MODEL_GRADED,
            "Model-graded review",
            "Asks a grading provider to score the change; needs that provider configured.",
            graded,
            review_oracle_requirement(),
            review_type_explanations(),
        ),
    ]
}

/// Registers the whole review pack into whatever catalog it is handed.
fn register_review_topologies(catalog: &mut TopologyCatalog) -> Result<(), TopologyCatalogError> {
    register_review_topologies_in_order(catalog, false)
}

/// Registers the review pack, optionally reversing its registration order.
fn register_review_topologies_in_order(
    catalog: &mut TopologyCatalog,
    reversed: bool,
) -> Result<(), TopologyCatalogError> {
    register_review_contract(catalog)?;
    let mut registrations = review_registrations();
    if reversed {
        registrations.reverse();
    }
    for registration in registrations {
        catalog.register_complete(registration)?;
    }
    Ok(())
}

/// Registers one registration whose corpus resource has no stable explanation.
fn register_unexplained_topology(
    catalog: &mut TopologyCatalog,
) -> Result<(), TopologyCatalogError> {
    let mut graph = Graph::new();
    graph.add_system(unexplained_review);
    catalog.register_complete(review_registration(
        UNEXPLAINED,
        "Unexplained review",
        "Reads a corpus resource the pack forgot to bind to a stable key.",
        graph,
        CandidateRequirements::none(),
        review_type_explanations(),
    ))?;
    Ok(())
}

/// The independent second pack, as an ordinary Polaris plugin.
#[derive(Debug, Default, Copy, Clone)]
struct ReviewTopologyPackPlugin;

#[plugin(id = "polaris_review::change_review_pack", version = "0.1.0")]
impl Plugin for ReviewTopologyPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        if let Err(error) = register_review_topologies(&mut catalog) {
            panic!("review topology pack registration failed: {error}");
        }
    }
}

/// A pack whose one registration is missing a type explanation.
#[derive(Debug, Default, Copy, Clone)]
struct MalformedReviewPackPlugin;

#[plugin(id = "polaris_review::malformed_review_pack", version = "0.1.0")]
impl Plugin for MalformedReviewPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        let build = register_review_contract(&mut catalog)
            .and_then(|()| register_unexplained_topology(&mut catalog));
        if let Err(error) = build {
            panic!("malformed review pack registration failed: {error}");
        }
    }
}

/// Registers one review registration under its own conclusion key.
fn register_conflicting_topology(
    catalog: &mut TopologyCatalog,
) -> Result<(), TopologyCatalogError> {
    register_review_contract(catalog)?;
    let mut explanations = TypeExplanationRegistry::new();
    explanations
        .bind_output::<AssessmentConclusion>(review_type_key(
            "polaris-review.assessment-conclusion",
        ))
        .expect("the review pack may name the type in its own registry");
    let mut graph = Graph::new();
    graph.add_system(single_pass_review);
    catalog.register_complete(review_registration(
        SINGLE_PASS,
        "Single-pass review",
        "One read and one verdict; lowest latency and no second opinion.",
        graph,
        CandidateRequirements::none(),
        explanations,
    ))?;
    Ok(())
}

/// A pack that renames a native type the coding pack already explained.
#[derive(Debug, Default, Copy, Clone)]
struct ConflictingReviewPackPlugin;

#[plugin(id = "polaris_review::conflicting_review_pack", version = "0.1.0")]
impl Plugin for ConflictingReviewPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        if let Err(error) = register_conflicting_topology(&mut catalog) {
            panic!("conflicting review pack registration failed: {error}");
        }
    }
}

// ===========================================================================
// Host side. Nothing below may name a pack.
// ===========================================================================

const LIB_RS: &str = "\
//! A tiny crate with one unimplemented helper.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    todo!()
}
";

fn project() -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    let task = CodingTask::new(
        "review-multi-pack",
        dir.path(),
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}",
        "todo!()",
        CodingIntent::Careful,
    )
    .expect("the coding task is well formed");
    (dir, task)
}

fn availability_id() -> AvailabilitySnapshotId {
    "c0c0c0c0-0000-4000-8000-000000000001"
        .parse()
        .expect("availability ID is a UUIDv4")
}

fn decision_id(suffix: &str) -> SelectionDecisionId {
    format!("c0c0c0c0-0000-4000-8000-0000000000{suffix}")
        .parse()
        .expect("decision ID is a UUIDv4")
}

/// Assembles a server with both packs, in the requested plugin order.
async fn assemble(review_first: bool) -> Server {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    if review_first {
        server.add_plugins(ReviewTopologyPackPlugin);
        server.add_plugins(CodingTopologyPackPlugin);
    } else {
        server.add_plugins(CodingTopologyPackPlugin);
        server.add_plugins(ReviewTopologyPackPlugin);
    }
    server
        .finish()
        .await
        .expect("two independent packs assemble into one catalog");
    server
}

fn sealed(server: &Server) -> TopologyCatalogSnapshot {
    server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed")
}

fn registration_ids(snapshot: &TopologyCatalogSnapshot) -> Vec<String> {
    snapshot
        .candidates()
        .iter()
        .map(|candidate| candidate.registration_id().as_str().to_owned())
        .collect()
}

/// Exclusion codes per registration, in canonical registration order.
fn exclusion_codes(admission: &CandidateAdmissionSet) -> BTreeMap<String, Vec<String>> {
    admission
        .evaluations()
        .iter()
        .map(|evaluation| {
            (
                evaluation.candidate().registration_id().as_str().to_owned(),
                evaluation
                    .result()
                    .reasons()
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect(),
            )
        })
        .collect()
}

fn empty_statistics() -> TopologyStatsSnapshot {
    TopologyEvidence::new(std::sync::Arc::new(BoundedInMemoryEvidence::new(
        policy::evidence_config(),
    )))
    .capture_statistics(&policy::statistics_request())
    .expect("an empty evidence store captures empty statistics")
}

/// The whole host-side selection path, parameterized only by a contract key.
///
/// This is the load-bearing function of the proof. It has no pack parameter, no
/// downcast, and no branch on any registration, provenance, descriptor, or
/// contract identity. Every candidate-specific value it uses —  requirements,
/// stable signature explanations, resolved configuration, and author priors —
/// is read out of the sealed catalog by `preflight_catalog` and
/// `policy::candidate_facts`.
fn select_under(
    snapshot: &TopologyCatalogSnapshot,
    contract: &TopologyContractKey,
    availability: &RuntimeAvailabilitySnapshot,
    profile: &TaskProfile,
    intent: CodingIntent,
    decision: SelectionDecisionId,
) -> (CandidateAdmissionSet, SelectionDecision) {
    let admission = preflight_catalog(snapshot, contract, availability, false)
        .expect("generic preflight reads every candidate from the sealed catalog");
    let objective = CodingObjective::new(intent)
        .build()
        .expect("the objective builds");
    let inputs = SelectionInputs::new(
        decision,
        profile.clone(),
        objective,
        contract.clone(),
        snapshot.clone(),
        availability.clone(),
        admission.clone(),
        empty_statistics(),
        policy::candidate_facts(snapshot, None).expect("selector facts build from the catalog"),
        policy::selector_config().expect("the selector configuration builds"),
    );
    let decision = deterministic::select(&inputs).expect("selection completes");
    (admission, decision)
}

fn winner(decision: &SelectionDecision) -> String {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => {
            selected.registration_id().as_str().to_owned()
        }
        SelectionOutcome::NoEligibleCandidate { diagnostics } => {
            panic!("expected a winner, got diagnostics {diagnostics:?}")
        }
    }
}

/// Every objective this proof ranks under, with a stable decision ID each.
const OBJECTIVES: [(&str, CodingIntent, &str); 3] = [
    ("fast", CodingIntent::Fast, "11"),
    ("careful", CodingIntent::Careful, "12"),
    ("thorough", CodingIntent::Thorough, "13"),
];

// ===========================================================================
// 1. Both packs land in one sealed catalog.
// ===========================================================================

#[tokio::test]
async fn two_independent_packs_appear_in_one_sealed_snapshot() {
    let server = assemble(false).await;
    let snapshot = sealed(&server);

    // Seven coding registrations and four review registrations, one catalog.
    assert_eq!(snapshot.candidates().len(), 11);
    assert_eq!(snapshot.contracts().len(), 2);

    let providers: Vec<_> = snapshot
        .candidates()
        .iter()
        .map(|candidate| candidate.provenance().provider().to_owned())
        .collect();
    assert_eq!(
        providers
            .iter()
            .filter(|provider| provider.as_str() == REVIEW_PROVIDER)
            .count(),
        4,
        "the review pack must contribute four registrations, got {providers:?}"
    );
    assert_eq!(
        providers
            .iter()
            .filter(|provider| provider.as_str() == "polaris-code.coding-topology-pack")
            .count(),
        7
    );

    // Complete registration is a catalog-wide invariant, not a per-pack one.
    for candidate in snapshot.candidates() {
        assert!(
            candidate.admission_metadata().is_some(),
            "{} sealed without complete admission metadata",
            candidate.registration_id().as_str()
        );
    }

    // Host assembly is one plugin per pack, not one edge per topology.
    let manifest = server.plugin_manifest();
    for pack in [
        PluginId::new(CodingTopologyPackPlugin::ID),
        PluginId::new(ReviewTopologyPackPlugin::ID),
    ] {
        let entry = manifest.entry(&pack).expect("the host enables this pack");
        assert_eq!(entry.extends().len(), 1);
        assert_eq!(
            entry.extends()[0].provider(),
            Some(&PluginId::new(TopologyCatalogPlugin::ID)),
            "a pack must bind to the catalog capability, not to another pack"
        );
    }
}

// ===========================================================================
// 2. Insertion order is irrelevant.
// ===========================================================================

#[tokio::test]
async fn pack_insertion_order_does_not_change_the_sealed_catalog() {
    let coding_first = assemble(false).await;
    let review_first = assemble(true).await;
    let forward = sealed(&coding_first);
    let reverse = sealed(&review_first);

    assert_eq!(forward.content_digest(), reverse.content_digest());
    assert_eq!(forward.generation(), reverse.generation());
    assert_eq!(registration_ids(&forward), registration_ids(&reverse));
    assert_eq!(
        forward
            .contracts()
            .iter()
            .map(|contract| contract.key().clone())
            .collect::<Vec<_>>(),
        reverse
            .contracts()
            .iter()
            .map(|contract| contract.key().clone())
            .collect::<Vec<_>>()
    );
    // Canonical order is registration identity, not arrival order.
    let mut sorted = registration_ids(&forward);
    sorted.sort();
    assert_eq!(registration_ids(&forward), sorted);
}

#[tokio::test]
async fn pack_insertion_order_does_not_change_selection_outcomes() {
    let (_dir, task) = project();
    let profile = task.profile().expect("the profile aggregates");
    let availability = probe_availability(
        &task,
        availability_id(),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");

    let coding_first = assemble(false).await;
    let review_first = assemble(true).await;
    let forward = sealed(&coding_first);
    let reverse = sealed(&review_first);

    for contract in [coding_contract_key(), review_contract_key()] {
        for (label, intent, suffix) in OBJECTIVES {
            let (forward_admission, forward_decision) = select_under(
                &forward,
                &contract,
                &availability,
                &profile,
                intent,
                decision_id(suffix),
            );
            let (reverse_admission, reverse_decision) = select_under(
                &reverse,
                &contract,
                &availability,
                &profile,
                intent,
                decision_id(suffix),
            );
            assert_eq!(
                exclusion_codes(&forward_admission),
                exclusion_codes(&reverse_admission),
                "mechanical admission differed by insertion order for {label}"
            );
            assert_eq!(
                forward_decision.fingerprint(),
                reverse_decision.fingerprint(),
                "the {label} decision fingerprint differed by insertion order"
            );
            assert_eq!(forward_decision, reverse_decision);
        }
    }
}

// ===========================================================================
// 3. A malformed registration stops assembly before preflight.
// ===========================================================================

#[test]
fn one_missing_type_explanation_stops_assembly_before_preflight() {
    let (_dir, task) = project();
    let availability = probe_availability(
        &task,
        availability_id(),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");

    let mut catalog = TopologyCatalog::new();
    register_review_topologies(&mut catalog).expect("the well-formed review pack registers");
    let before = catalog.len();
    let before_generation = catalog.generation();

    let error = register_unexplained_topology(&mut catalog)
        .expect_err("an unexplained boundary must be rejected at registration");

    // The error is typed and actionable: it names the registration and every
    // missing channel plus its ephemeral native label.
    let TopologyCatalogError::UnexplainedSignature {
        registration,
        missing,
    } = &error
    else {
        panic!("expected UnexplainedSignature, got {error:?}");
    };
    assert_eq!(registration.as_str(), UNEXPLAINED);
    assert_eq!(missing.len(), 1);
    assert!(
        missing[0].display_type().contains("UnexplainedCorpus"),
        "the missing boundary must name the offending native type, got {:?}",
        missing[0].display_type()
    );
    assert!(error.to_string().contains("unexplained signature boundary"));

    // The failed insertion left the catalog untouched.
    assert_eq!(catalog.len(), before);
    assert_eq!(catalog.generation(), before_generation);

    // And because it never entered the catalog, preflight cannot see it: the
    // malformed registration is not a runtime exclusion, it does not exist.
    let snapshot = catalog
        .seal_admission_ready()
        .expect("the remaining registrations are all admission-ready");
    let admission = preflight_catalog(&snapshot, &review_contract_key(), &availability, false)
        .expect("preflight runs over the sealed catalog");
    assert!(
        !exclusion_codes(&admission).contains_key(UNEXPLAINED),
        "a rejected registration must never reach mechanical preflight"
    );
    assert_eq!(admission.evaluations().len(), 4);
}

#[tokio::test]
#[should_panic(expected = "unexplained signature boundary")]
async fn a_malformed_pack_fails_server_assembly_rather_than_sealing() {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(CodingTopologyPackPlugin);
    server.add_plugins(MalformedReviewPackPlugin);
    // One malformed registration in one pack stops the whole assembly; a
    // well-formed pack beside it does not rescue the catalog into `ready`.
    let _ = server.finish().await;
}

// ===========================================================================
// 4. The selector reads both packs through the catalog alone.
// ===========================================================================

#[tokio::test]
async fn the_selector_reads_both_packs_through_the_catalog_alone() {
    let (_dir, task) = project();
    let profile = task.profile().expect("the profile aggregates");
    let availability = probe_availability(
        &task,
        availability_id(),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");
    let server = assemble(false).await;
    let snapshot = sealed(&server);

    // `candidate_facts` is production `polaris_code` code. It takes the sealed
    // snapshot and nothing else, and it returns facts for every candidate of
    // both packs.
    let facts = policy::candidate_facts(&snapshot, None).expect("selector facts build");
    assert_eq!(facts.len(), 11);
    let fact_ids: Vec<_> = facts
        .iter()
        .map(|candidate| candidate.registration_id().as_str().to_owned())
        .collect();
    assert_eq!(fact_ids, registration_ids(&snapshot));

    // The host never wrote a review prior. These components reached scoring by
    // travelling through the catalog's admission metadata alone.
    let (_, decision) = select_under(
        &snapshot,
        &review_contract_key(),
        &availability,
        &profile,
        CodingIntent::Careful,
        decision_id("31"),
    );
    let scored: BTreeMap<_, _> = decision
        .candidates()
        .iter()
        .map(|consideration| {
            (
                consideration
                    .candidate()
                    .registration_id()
                    .as_str()
                    .to_owned(),
                consideration
                    .score()
                    .map(|score| score.components().to_vec())
                    .unwrap_or_default(),
            )
        })
        .collect();
    for id in [SINGLE_PASS, THOROUGH_PASS] {
        let priors: Vec<_> = scored[id]
            .iter()
            .filter(|component| component.source() == ScoreSource::AuthorPrior)
            .collect();
        assert_eq!(priors.len(), 4, "{id} lost its catalog-owned priors");
        for component in &priors {
            assert!(
                component.component_id().as_str().starts_with(id),
                "{id} was scored with a component it does not own: {}",
                component.component_id()
            );
        }
        // Everything else in the score is the selector's own pessimistic
        // gap-filling under `MissingPreferencePolicy`, not a pack contribution.
        for component in &scored[id] {
            assert!(
                component.source() == ScoreSource::AuthorPrior
                    || component
                        .component_id()
                        .as_str()
                        .starts_with("policy.missing."),
                "an unexpected score source reached {id}: {}",
                component.component_id()
            );
        }
    }
    // Mechanically excluded candidates are never scored, in either pack.
    assert!(scored[CORPUS_SCAN].is_empty());
    assert!(scored[MODEL_GRADED].is_empty());
    assert!(scored[topology::PREDICTION_ERROR].is_empty());

    // Every one of these structures is catalog-owned, for a pack the host
    // cannot name.
    for id in [SINGLE_PASS, THOROUGH_PASS, CORPUS_SCAN, MODEL_GRADED] {
        let candidate = snapshot
            .candidates()
            .iter()
            .find(|candidate| candidate.registration_id().as_str() == id)
            .expect("the review candidate is in the sealed catalog");
        let metadata = candidate
            .admission_metadata()
            .expect("the review candidate registered completely");
        assert_eq!(metadata.selection().registration_id().as_str(), id);
        assert_eq!(
            metadata.configuration_schema().id().as_str(),
            "polaris-review.review-configuration",
            "the review pack keeps its own configuration schema identity"
        );
        assert!(
            metadata
                .signature_explanation()
                .produced_outputs()
                .contains(&coding_owned_conclusion_key())
        );
        for component in metadata.author_priors() {
            assert_eq!(component.source(), ScoreSource::AuthorPrior);
        }
    }
}

#[tokio::test]
async fn one_generic_selection_driver_serves_both_packs() {
    let (_dir, task) = project();
    let profile = task.profile().expect("the profile aggregates");
    let availability = probe_availability(
        &task,
        availability_id(),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");
    let server = assemble(false).await;
    let snapshot = sealed(&server);

    // --- the review contract ------------------------------------------------
    let (review_admission, review_fast) = select_under(
        &snapshot,
        &review_contract_key(),
        &availability,
        &profile,
        CodingIntent::Fast,
        decision_id("21"),
    );
    let review_codes = exclusion_codes(&review_admission);

    // Preflight evaluated all eleven candidates from both packs in one pass.
    assert_eq!(review_admission.evaluations().len(), 11);
    assert!(review_codes[SINGLE_PASS].is_empty());
    assert!(review_codes[THOROUGH_PASS].is_empty());
    // Catalog-owned signature explanations and declared requirements — written
    // by a pack the host never compiled against — drive both exclusions.
    assert_eq!(
        review_codes[CORPUS_SCAN],
        vec!["resource.missing".to_owned()]
    );
    assert_eq!(
        review_codes[MODEL_GRADED],
        vec!["provider.unavailable".to_owned()]
    );
    // The coding pack's registrations are excluded only for claiming another
    // contract, which is the generic contract filter, not a pack branch.
    for id in [
        topology::DIRECT,
        topology::PREDICTION_ERROR,
        topology::WORKLIST_SWEEP,
        topology::COVERAGE_AUDIT,
    ] {
        assert!(
            review_codes[id].contains(&"contract.not-requested".to_owned()),
            "{id} should be filtered out by contract, got {:?}",
            review_codes[id]
        );
    }

    // --- the coding contract, same driver -----------------------------------
    let (coding_admission, coding_fast) = select_under(
        &snapshot,
        &coding_contract_key(),
        &availability,
        &profile,
        CodingIntent::Fast,
        decision_id("22"),
    );
    let coding_codes = exclusion_codes(&coding_admission);
    assert_eq!(coding_admission.evaluations().len(), 11);
    assert!(coding_codes[topology::DIRECT].is_empty());
    for id in [SINGLE_PASS, THOROUGH_PASS, CORPUS_SCAN, MODEL_GRADED] {
        assert!(coding_codes[id].contains(&"contract.not-requested".to_owned()));
    }

    // Each contract selects out of its own pack, through one driver.
    assert_eq!(winner(&review_fast), SINGLE_PASS);
    assert_eq!(winner(&coding_fast), topology::DIRECT);

    // The objective really reranks the second pack too: quality- and
    // coverage-weighted objectives prefer the thorough review.
    for (label, intent, suffix) in OBJECTIVES {
        let (_, decision) = select_under(
            &snapshot,
            &review_contract_key(),
            &availability,
            &profile,
            intent,
            decision_id(suffix),
        );
        let expected = if intent == CodingIntent::Fast {
            SINGLE_PASS
        } else {
            THOROUGH_PASS
        };
        assert_eq!(
            winner(&decision),
            expected,
            "objective {label} picked wrong"
        );
        // Every candidate of both packs is accounted for exactly once.
        assert_eq!(decision.candidates().len(), 11);
    }
}

// ===========================================================================
// 5. The recorded consumer seam.
// ===========================================================================

#[tokio::test]
#[should_panic(expected = "conflicting type explanations")]
async fn two_packs_must_agree_on_the_stable_key_of_a_shared_native_type() {
    // `ConflictingReviewPackPlugin` is the same review pack with one change:
    // it binds the Cosmos-owned `AssessmentConclusion` under its own namespace
    // instead of copying the coding pack's private literal. One of the two
    // packs then fails `register_complete`, and which one depends on build
    // order, so only the shared typed cause is asserted here.
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(CodingTopologyPackPlugin);
    server.add_plugins(ConflictingReviewPackPlugin);
    let _ = server.finish().await;
}

#[test]
fn the_conflicting_binding_is_a_typed_catalog_error() {
    // The same conflict, reached without a server so the typed error is
    // inspectable: one catalog, two registrations, one native type, two keys.
    let mut catalog = TopologyCatalog::new();
    register_review_topologies(&mut catalog).expect("the well-formed review pack registers");
    let before = catalog.len();

    let mut renamed = TypeExplanationRegistry::new();
    renamed
        .bind_output::<AssessmentConclusion>(review_type_key(
            "polaris-review.assessment-conclusion",
        ))
        .expect("the rename is legal inside one registration's own registry");
    let mut graph = Graph::new();
    graph.add_system(thorough_review);
    let error = catalog
        .register_complete(review_registration(
            "review.renamed-conclusion",
            "Renamed conclusion",
            "Binds the shared conclusion type under a second stable key.",
            graph,
            CandidateRequirements::none(),
            renamed,
        ))
        .expect_err("a second stable key for one native type must be rejected");

    let TopologyCatalogError::AdmissionMetadataConflict { registration, .. } = &error else {
        panic!("expected AdmissionMetadataConflict, got {error:?}");
    };
    assert_eq!(registration.as_str(), "review.renamed-conclusion");
    assert!(error.to_string().contains("conflicting type explanations"));
    assert_eq!(
        catalog.len(),
        before,
        "the failed insertion changed nothing"
    );
}

// ===========================================================================
// 6. Non-vacuity guards.
// ===========================================================================

#[test]
fn registration_order_inside_one_pack_is_also_irrelevant() {
    // Plugin order is Polaris's to schedule; this pins the other half the host
    // controls — the order a pack itself calls `register_complete` in.
    let mut forward = TopologyCatalog::new();
    register_review_topologies_in_order(&mut forward, false).expect("forward order registers");
    let mut reverse = TopologyCatalog::new();
    register_review_topologies_in_order(&mut reverse, true).expect("reverse order registers");

    let forward = forward
        .seal_admission_ready()
        .expect("every forward registration is admission-ready");
    let reverse = reverse
        .seal_admission_ready()
        .expect("every reverse registration is admission-ready");
    assert_eq!(forward.content_digest(), reverse.content_digest());
    assert_eq!(forward.generation(), reverse.generation());
    assert_eq!(registration_ids(&forward), registration_ids(&reverse));
}

#[test]
fn no_production_source_mentions_the_second_pack() {
    // The claim "no per-pack host code" is only worth something if the host
    // really cannot name this pack. Nothing under `src/` may.
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    scan_rust_sources(&src, &mut |path, contents| {
        for needle in [
            "polaris-review",
            "polaris_review",
            "ReviewTopologyPack",
            "ChangeNote",
        ] {
            if contents.contains(needle) {
                offenders.push(format!("{} mentions {needle}", path.display()));
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "production source must not know about the second pack: {offenders:?}"
    );
}

fn scan_rust_sources(dir: &std::path::Path, visit: &mut impl FnMut(&std::path::Path, &str)) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            scan_rust_sources(&path, visit);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            visit(&path, &contents);
        }
    }
}

// ===========================================================================
// 7. Why the order tests are not vacuous, and the second recorded seam.
// ===========================================================================

/// Outcome of the contract-claiming probe, in build order.
static PROBE_OUTCOMES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A pack that tries to contribute a candidate to the *coding* pack's contract.
///
/// It owns no contract of its own, so its registration only succeeds if the
/// coding pack has already built. Whether that is true is Polaris's scheduling
/// decision, not the host's.
#[derive(Debug, Default, Copy, Clone)]
struct ContractClaimingPackPlugin;

#[plugin(id = "polaris_review::contract_claiming_pack", version = "0.1.0")]
impl Plugin for ContractClaimingPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        let mut graph = Graph::new();
        graph.add_system(single_pass_review);
        let descriptor = TopologyDescriptor::new(
            TopologyKey::new(
                TopologyId::new("review.borrowed-contract").expect("topology ID is valid"),
                review_version(),
            ),
            "Borrowed-contract review",
            "Contributes a candidate to a contract another pack owns.",
        )
        .expect("descriptor metadata is valid")
        .claim(coding_contract_key());
        let selection = RegistrationSelectionRef::new(
            RegistrationId::new("review.borrowed-contract").expect("registration ID is valid"),
            descriptor.key().clone(),
            ContentDigest::sha256(b"polaris-review.revision:borrowed"),
            review_configuration("review.borrowed-contract"),
        )
        .expect("selection metadata is valid");
        let outcome = catalog.register_complete(CompleteTopologyRegistration::new(
            selection,
            descriptor,
            graph,
            RegistrationProvenance::new(REVIEW_PROVIDER).expect("review provider ID is valid"),
            CandidateRequirements::none(),
            review_type_explanations(),
            Vec::new(),
        ));
        let recorded = match outcome {
            Ok(_) => "registered".to_owned(),
            Err(error) => error.to_string(),
        };
        PROBE_OUTCOMES
            .lock()
            .expect("the probe outcome lock is not poisoned")
            .push(recorded);
    }
}

async fn claiming_probe(claimer_first: bool) -> (String, usize) {
    PROBE_OUTCOMES
        .lock()
        .expect("the probe outcome lock is not poisoned")
        .clear();
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    if claimer_first {
        server.add_plugins(ContractClaimingPackPlugin);
        server.add_plugins(CodingTopologyPackPlugin);
    } else {
        server.add_plugins(CodingTopologyPackPlugin);
        server.add_plugins(ContractClaimingPackPlugin);
    }
    server.finish().await.expect("both packs assemble");
    let outcome = PROBE_OUTCOMES
        .lock()
        .expect("the probe outcome lock is not poisoned")
        .first()
        .cloned()
        .expect("the probe pack built exactly once");
    (outcome, sealed(&server).candidates().len())
}

#[tokio::test]
async fn sibling_pack_build_order_is_not_the_host_add_plugins_order() {
    // One test, two assemblies, because they share the probe's recording slot.
    let (claimer_first_outcome, claimer_first_count) = claiming_probe(true).await;
    let (coding_first_outcome, coding_first_count) = claiming_probe(false).await;

    // The two host orders genuinely build the packs in opposite orders. That is
    // what stops `pack_insertion_order_does_not_change_*` from being vacuous:
    // the sealed catalog and every decision fingerprint are identical *despite*
    // the packs really running in different sequences.
    assert_ne!(
        claimer_first_outcome, coding_first_outcome,
        "both host orders produced the same pack build order, so the order tests prove nothing"
    );
    let mut counts = [claimer_first_count, coding_first_count];
    counts.sort_unstable();
    assert_eq!(counts, [7, 8]);

    // Exactly one direction saw the coding contract already registered. In this
    // assembly that is the direction where the coding pack is added *second*,
    // which is the opposite of the host's `add_plugins` order — sibling
    // extenders are scheduled by Polaris, not by the host.
    let claimed = [&claimer_first_outcome, &coding_first_outcome]
        .into_iter()
        .filter(|outcome| outcome.as_str() == "registered")
        .count();
    assert_eq!(claimed, 1);
    let refused = [&claimer_first_outcome, &coding_first_outcome]
        .into_iter()
        .find(|outcome| outcome.as_str() != "registered")
        .expect("one direction must be refused");
    assert!(
        refused.contains("claims unknown contract"),
        "expected a typed unknown-contract refusal, got {refused}"
    );

    // Consequence, recorded as a seam: a pack cannot contribute a candidate to
    // a contract another pack owns, because whether that contract exists when
    // it builds is not something the host can order. Packs that must coexist
    // therefore own their own contracts, as the review pack above does.
}
