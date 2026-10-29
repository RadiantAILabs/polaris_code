//! Behavioural proof for the approved cognition blocks and model points.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polaris::graph::hooks::HooksAPI;
use polaris::graph::hooks::events::GraphEvent;
use polaris::graph::node::Node;
use polaris::graph::{Graph, GraphExecutor, GraphSignature};
use polaris::system::param::{Out, Res, ResMut, SystemContext};
use polaris::system::plugin::{Schedule, Version};
use polaris::system::resource::LocalResource;
use polaris::system::system;
use polaris::system::system::erase;
use serde::{Deserialize, Serialize};

use polaris_cosmos::contracts::{
    Assessment, AssessmentAccounting, AssessmentConclusion, AssessmentSummary, ConclusionKind,
    ContentDigest, ContextTypeId, ContextTypeKey, EpistemicBasis, EvidenceKind, EvidenceObjectId,
    EvidenceRef, EvidenceStoreId, PolicyId, PolicyRef, Provenance, ResidualDisposition, SourceId,
    SourceKey, SourceKind, Status, StopReason, SubjectSchema, SubjectSchemaId, SubjectSchemaKey,
};
use polaris_cosmos::topology::{
    ConfigurationDigest, RegistrationId, RegistrationProvenance, StableSignatureExplanation,
    TopologyCatalog, TopologyContract, TopologyContractId, TopologyContractKey, TopologyDescriptor,
    TopologyId, TopologyKey, TypeExplanationRegistry, UnexplainedBoundary,
};

mod contracts {
    pub use polaris_cosmos::contracts::*;
}

mod topology {
    pub use polaris_cosmos::topology::*;
}

/// Stand-in for the existing schedule when `approved.rs` is compiled in isolation.
#[derive(Debug, Clone, Copy)]
pub struct OnVerify;
impl Schedule for OnVerify {}

#[allow(dead_code)]
#[path = "../src/cognition/approved.rs"]
mod approved;

use approved::bounded_correction;
use approved::coverage_sweep;
use approved::{
    OnAdjudicate, OnCombine, OnEscalate, OnProbe, OnResolve, OnScope, OracleQualification,
    OracleWaiver, ResolutionInput,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CodeCriterion {
    id: String,
    status: Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodeObservation(String);

#[derive(Debug)]
struct CodePlan {
    scope: Vec<CodeCriterion>,
    correction: VecDeque<Status>,
    probes: usize,
    produced: usize,
}
impl LocalResource for CodePlan {}

fn code_schema() -> SubjectSchema<CodeCriterion> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("fixtures.code-criterion").unwrap(),
            Version::new(1, 0, 0),
        ),
        "A repository criterion and the status its deterministic fixture should emit.",
        r#"{"type":"object","required":["id","status"]}"#,
        |subject: &CodeCriterion| {
            if subject.id.is_empty() {
                Err("criterion ID must not be empty".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .unwrap()
}

fn evidence(id: &str) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceStoreId::new("fixtures.evidence").unwrap(),
        EvidenceKind::VerifierOutput,
        EvidenceObjectId::new(id).unwrap(),
    )
}

fn qualification() -> OracleQualification {
    OracleQualification::rejected_negative_control(evidence("known-negative-rejected"))
}

fn provenance(id: &str) -> Provenance {
    Provenance::new(
        SourceKey::new(
            SourceId::new("fixtures.verifier").unwrap(),
            Version::new(1, 0, 0),
        ),
        SourceKind::AutomatedVerifier,
        EpistemicBasis::Verified,
    )
    .with_evidence(evidence(id))
}

fn policy(id: &str) -> PolicyRef {
    PolicyRef::new(PolicyId::new(id).unwrap(), Version::new(1, 0, 0))
}

fn assessment(subject: CodeCriterion) -> Assessment<CodeCriterion> {
    let status = subject.status;
    let evidence_id = format!("assessment-{}", subject.id);
    Assessment::new(
        &code_schema(),
        subject,
        status,
        vec![provenance(&evidence_id)],
    )
    .unwrap()
}

fn summarize(input: &ResolutionInput<CodeCriterion>) -> AssessmentSummary {
    AssessmentSummary::new(
        input.assessments(),
        policy("fixtures.combine"),
        ContentDigest::sha256(b"fixtures-combine-v1"),
        vec![provenance("combination")],
    )
    .unwrap()
}

fn conclude(
    input: &ResolutionInput<CodeCriterion>,
    summary: AssessmentSummary,
) -> AssessmentConclusion {
    let accounting = input
        .assessments()
        .iter()
        .filter(|item| matches!(item.status(), Status::Issue | Status::Indeterminate))
        .map(|item| {
            let disposition = if input.stop_reason() == StopReason::WorkExhausted
                && item.status() == Status::Issue
            {
                ResidualDisposition::Resolved
            } else if input.stop_reason() == StopReason::EscalatedOut {
                ResidualDisposition::Blocked
            } else {
                ResidualDisposition::Deferred
            };
            AssessmentAccounting::new(
                item.content_digest(),
                disposition,
                policy("fixtures.resolve"),
                ContentDigest::sha256(b"fixtures-resolve-v1"),
                vec![provenance(&format!("account-{}", item.subject().id))],
            )
            .unwrap()
        })
        .collect();
    AssessmentConclusion::new(
        summary,
        accounting,
        input.stop_reason(),
        input.assessments(),
    )
    .unwrap()
}

#[system]
async fn code_scope(plan: Res<CodePlan>) -> Vec<CodeCriterion> {
    plan.scope.clone()
}

#[system]
async fn empty_scope_assessment(_plan: Res<CodePlan>) -> Assessment<CodeCriterion> {
    assessment(CodeCriterion {
        id: "empty-scope".to_owned(),
        status: Status::NotApplicable,
    })
}

#[system]
async fn code_probe(criterion: Out<CodeCriterion>, mut plan: ResMut<CodePlan>) -> CodeObservation {
    plan.probes += 1;
    CodeObservation(format!("observed:{}", criterion.id))
}

#[system]
async fn code_adjudicate(
    criterion: Out<CodeCriterion>,
    observation: Out<CodeObservation>,
) -> Assessment<CodeCriterion> {
    assert!(observation.0.ends_with(&criterion.id));
    assessment(criterion.clone())
}

#[system]
async fn assess_code_gaps(pending: Out<Vec<CodeCriterion>>) -> Vec<Assessment<CodeCriterion>> {
    pending
        .iter()
        .cloned()
        .map(|mut criterion| {
            criterion.status = Status::Indeterminate;
            assessment(criterion)
        })
        .collect()
}

#[system]
async fn combine_code(input: Out<ResolutionInput<CodeCriterion>>) -> AssessmentSummary {
    summarize(&input)
}

#[system]
async fn resolve_code(
    input: Out<ResolutionInput<CodeCriterion>>,
    summary: Out<AssessmentSummary>,
) -> AssessmentConclusion {
    conclude(&input, summary.clone())
}

#[system]
async fn escalate_code(
    input: Out<ResolutionInput<CodeCriterion>>,
    summary: Out<AssessmentSummary>,
) -> AssessmentConclusion {
    assert_eq!(input.stop_reason(), StopReason::EscalatedOut);
    conclude(&input, summary.clone())
}

#[system]
async fn correction_produce(mut plan: ResMut<CodePlan>) -> CodeCriterion {
    let status = plan.correction.pop_front().unwrap_or(Status::Issue);
    plan.produced += 1;
    CodeCriterion {
        id: format!("round-{}", plan.produced),
        status,
    }
}

#[system]
async fn correction_judge(subject: Out<CodeCriterion>) -> Assessment<CodeCriterion> {
    assessment(subject.clone())
}

fn coverage_graph() -> Graph {
    let mut graph = Graph::new();
    coverage_sweep::build(
        &mut graph,
        coverage_sweep::Stages {
            scope: erase(code_scope),
            empty_scope: erase(empty_scope_assessment),
            probe: erase(code_probe),
            adjudicate: erase(code_adjudicate),
            assess_gaps: erase(assess_code_gaps),
            combine: erase(combine_code),
            resolve: erase(resolve_code),
        },
        qualification(),
    );
    graph
}

fn correction_graph() -> Graph {
    let mut graph = Graph::new();
    bounded_correction::build(
        &mut graph,
        bounded_correction::Stages {
            produce: erase(correction_produce),
            judge: erase(correction_judge),
            combine: erase(combine_code),
            resolve: erase(resolve_code),
            escalate: erase(escalate_code),
        },
        qualification(),
    );
    graph
}

fn code_plan(scope: Vec<CodeCriterion>, correction: impl IntoIterator<Item = Status>) -> CodePlan {
    CodePlan {
        scope,
        correction: correction.into_iter().collect(),
        probes: 0,
        produced: 0,
    }
}

fn criteria(statuses: &[Status]) -> Vec<CodeCriterion> {
    statuses
        .iter()
        .enumerate()
        .map(|(index, status)| CodeCriterion {
            id: format!("criterion-{index}"),
            status: *status,
        })
        .collect()
}

fn count_schedule<S: Schedule>(hooks: &HooksAPI) -> Arc<AtomicUsize> {
    let count = Arc::new(AtomicUsize::new(0));
    let observer_count = Arc::clone(&count);
    hooks
        .register_observer::<S, _>(std::any::type_name::<S>(), move |event| {
            if matches!(event, GraphEvent::SystemStart { .. }) {
                observer_count.fetch_add(1, Ordering::SeqCst);
            }
        })
        .unwrap();
    count
}

#[tokio::test]
async fn coverage_visits_every_status_and_emits_roles_per_subject() {
    let graph = coverage_graph();
    let validation = graph.validate();
    assert!(validation.is_ok(), "coverage graph invalid: {validation}");
    let diff = graph
        .signature()
        .satisfies_diff(&coverage_sweep::contract_signature::<
            CodeCriterion,
            CodeObservation,
        >());
    assert!(diff.is_empty(), "coverage signature drifted: {diff}");

    let statuses = [
        Status::Pass,
        Status::Issue,
        Status::NotApplicable,
        Status::Indeterminate,
    ];
    let mut context = SystemContext::new();
    coverage_sweep::prepare_context::<CodeCriterion>(
        &mut context,
        coverage_sweep::Config::new(8, 8, 8, StopReason::BudgetExhausted).unwrap(),
    )
    .unwrap();
    context.insert(code_plan(criteria(&statuses), []));

    let hooks = HooksAPI::new();
    let scopes = count_schedule::<OnScope>(&hooks);
    let probes = count_schedule::<OnProbe>(&hooks);
    let adjudications = count_schedule::<OnAdjudicate>(&hooks);
    let combinations = count_schedule::<OnCombine>(&hooks);
    let resolutions = count_schedule::<OnResolve>(&hooks);
    let result = GraphExecutor::new()
        .execute(&graph, &mut context, Some(&hooks), None)
        .await
        .unwrap();
    let conclusion = result.output::<AssessmentConclusion>().unwrap();

    assert_eq!(conclusion.stop_reason(), StopReason::WorkExhausted);
    assert_eq!(conclusion.summary().counts_by_status().pass(), 1);
    assert_eq!(conclusion.summary().counts_by_status().issue(), 1);
    assert_eq!(conclusion.summary().counts_by_status().not_applicable(), 1);
    assert_eq!(conclusion.summary().counts_by_status().indeterminate(), 1);
    assert_eq!(scopes.load(Ordering::SeqCst), 1);
    assert_eq!(probes.load(Ordering::SeqCst), 4);
    assert_eq!(adjudications.load(Ordering::SeqCst), 4);
    assert_eq!(combinations.load(Ordering::SeqCst), 1);
    assert_eq!(resolutions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn coverage_empty_scope_and_bound_both_materialize_visible_results() {
    let graph = coverage_graph();

    let mut empty = SystemContext::new();
    coverage_sweep::prepare_context::<CodeCriterion>(
        &mut empty,
        coverage_sweep::Config::new(4, 4, 4, StopReason::IterationLimit).unwrap(),
    )
    .unwrap();
    empty.insert(code_plan(Vec::new(), []));
    let empty_result = GraphExecutor::new()
        .execute(&graph, &mut empty, None, None)
        .await
        .unwrap();
    let empty_conclusion = empty_result.output::<AssessmentConclusion>().unwrap();
    assert_eq!(empty_conclusion.stop_reason(), StopReason::WorkExhausted);
    assert_eq!(
        empty_conclusion
            .summary()
            .counts_by_status()
            .not_applicable(),
        1
    );

    let mut bounded = SystemContext::new();
    coverage_sweep::prepare_context::<CodeCriterion>(
        &mut bounded,
        coverage_sweep::Config::new(4, 4, 1, StopReason::IterationLimit).unwrap(),
    )
    .unwrap();
    bounded.insert(code_plan(
        criteria(&[Status::Issue, Status::Pass, Status::Pass]),
        [],
    ));
    let bounded_result = GraphExecutor::new()
        .execute(&graph, &mut bounded, None, None)
        .await
        .unwrap();
    let bounded_conclusion = bounded_result.output::<AssessmentConclusion>().unwrap();
    assert_eq!(bounded_conclusion.stop_reason(), StopReason::IterationLimit);
    assert_eq!(
        bounded_conclusion
            .summary()
            .input_assessment_digests()
            .len(),
        3
    );
    assert_eq!(
        bounded_conclusion
            .summary()
            .counts_by_status()
            .indeterminate(),
        2,
        "each unvisited subject becomes a gap assessment"
    );
    assert_eq!(
        bounded_conclusion.conclusion_kind(),
        ConclusionKind::Unresolved
    );
}

#[tokio::test]
async fn correction_routes_all_statuses_without_collapsing_or_silent_caps() {
    let graph = correction_graph();
    let validation = graph.validate();
    assert!(validation.is_ok(), "correction graph invalid: {validation}");
    let switch = graph
        .nodes()
        .iter()
        .find_map(|node| match node {
            Node::Switch(switch) => Some(switch),
            _ => None,
        })
        .expect("correction must contain status dispatch");
    assert_eq!(
        switch
            .cases
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>(),
        ["pass", "issue", "not-applicable", "indeterminate"]
    );
    assert!(switch.default.is_none(), "status dispatch has no default");
    let diff = graph
        .signature()
        .satisfies_diff(&bounded_correction::contract_signature::<CodeCriterion>());
    assert!(diff.is_empty(), "correction signature drifted: {diff}");

    let mut accepted = SystemContext::new();
    bounded_correction::prepare_context::<CodeCriterion>(
        &mut accepted,
        bounded_correction::Config::new(4, 4).unwrap(),
    )
    .unwrap();
    accepted.insert(code_plan(
        Vec::new(),
        [Status::Issue, Status::NotApplicable, Status::Pass],
    ));
    let accepted_hooks = HooksAPI::new();
    let verifications = count_schedule::<OnVerify>(&accepted_hooks);
    let accepted_resolutions = count_schedule::<OnResolve>(&accepted_hooks);
    let accepted_escalations = count_schedule::<OnEscalate>(&accepted_hooks);
    let accepted_result = GraphExecutor::new()
        .execute(&graph, &mut accepted, Some(&accepted_hooks), None)
        .await
        .unwrap();
    let accepted_conclusion = accepted_result.output::<AssessmentConclusion>().unwrap();
    assert_eq!(
        accepted_conclusion.stop_reason(),
        StopReason::PredicateSatisfied
    );
    assert_eq!(accepted_conclusion.summary().counts_by_status().issue(), 1);
    assert_eq!(
        accepted_conclusion
            .summary()
            .counts_by_status()
            .not_applicable(),
        1
    );
    assert_eq!(accepted_conclusion.summary().counts_by_status().pass(), 1);
    assert_eq!(verifications.load(Ordering::SeqCst), 3);
    assert_eq!(accepted_resolutions.load(Ordering::SeqCst), 1);
    assert_eq!(accepted_escalations.load(Ordering::SeqCst), 0);

    let hooks = HooksAPI::new();
    let escalations = count_schedule::<OnEscalate>(&hooks);
    let mut indeterminate = SystemContext::new();
    bounded_correction::prepare_context::<CodeCriterion>(
        &mut indeterminate,
        bounded_correction::Config::new(3, 3).unwrap(),
    )
    .unwrap();
    indeterminate.insert(code_plan(Vec::new(), [Status::Indeterminate]));
    let escalated_result = GraphExecutor::new()
        .execute(&graph, &mut indeterminate, Some(&hooks), None)
        .await
        .unwrap();
    let escalated = escalated_result.output::<AssessmentConclusion>().unwrap();
    assert_eq!(escalated.stop_reason(), StopReason::EscalatedOut);
    assert_eq!(escalated.conclusion_kind(), ConclusionKind::Escalated);
    assert_eq!(escalations.load(Ordering::SeqCst), 1);

    let mut capped = SystemContext::new();
    bounded_correction::prepare_context::<CodeCriterion>(
        &mut capped,
        bounded_correction::Config::new(2, 2).unwrap(),
    )
    .unwrap();
    capped.insert(code_plan(Vec::new(), [Status::Issue, Status::Issue]));
    let capped_result = GraphExecutor::new()
        .execute(&graph, &mut capped, None, None)
        .await
        .unwrap();
    let capped_conclusion = capped_result.output::<AssessmentConclusion>().unwrap();
    assert_eq!(capped_conclusion.stop_reason(), StopReason::BudgetExhausted);
    assert_eq!(capped_conclusion.accounting().len(), 2);
    assert_eq!(
        capped_conclusion.conclusion_kind(),
        ConclusionKind::Unresolved
    );
}

#[test]
fn descriptors_expose_oracle_policy_roles_limits_and_replacement_boundaries() {
    let contract = contract_key();
    let waiver = OracleWaiver::new(
        "fixtures.no-negative-control",
        Version::new(1, 2, 0),
        "historical oracle predates retained negative-control fixtures",
    )
    .unwrap();
    let descriptor =
        coverage_sweep::descriptor(contract, OracleQualification::Waiver(waiver.clone()));
    let metadata = descriptor.metadata();

    assert!(metadata.roles().contains(&"OnScope"));
    assert!(metadata.roles().contains(&"OnResolve"));
    assert!(
        metadata
            .intended_fit()
            .contains(&"systematic coverage required")
    );
    assert!(metadata.anti_fit().contains(&"tight latency limit"));
    assert!(!metadata.stop_reasons().contains(&StopReason::Saturated));
    assert!(!metadata.stop_reasons().contains(&StopReason::Stagnated));
    assert!(
        metadata
            .runtime_requirements()
            .contains(&"no map-collect, suspension, recursion, or public edges")
    );
    assert!(!metadata.configuration_schema().is_empty());
    assert!(metadata.author_priors()[1].contains("linear"));
    assert!(
        metadata
            .execution_characteristics()
            .contains(&"no concurrency")
    );
    assert!(!metadata.documentation().is_empty());
    assert_eq!(metadata.replacement_axis(), "systematic coverage policy");
    assert!(metadata.alternatives().contains(&"sampled sweep"));
    assert_eq!(metadata.oracle(), &OracleQualification::Waiver(waiver));
    assert!(
        descriptor
            .topology()
            .summary()
            .contains("negative-control-waiver=fixtures.no-negative-control@1.2.0"),
        "the catalog-visible descriptor must retain the waiver"
    );
    assert!(descriptor.topology().summary().contains("intended-fit="));
    assert!(descriptor.topology().summary().contains("execution="));
}

fn contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("fixtures.assess-task").unwrap(),
        Version::new(1, 0, 0),
    )
}

fn topology_descriptor(id: &str, contract: TopologyContractKey) -> TopologyDescriptor {
    TopologyDescriptor::new(
        TopologyKey::new(TopologyId::new(id).unwrap(), Version::new(1, 0, 0)),
        id,
        "direct routed embodiment pattern; one non-canonical low-overhead option",
    )
    .unwrap()
    .claim(contract)
}

async fn direct_conclusion() -> AssessmentConclusion {
    let item = assessment(CodeCriterion {
        id: "direct".to_owned(),
        status: Status::Pass,
    });
    let input = ResolutionInput::new(vec![item], Vec::new(), StopReason::PredicateSatisfied);
    let summary = summarize(&input);
    conclude(&input, summary)
}

fn shape(graph: &Graph) -> (usize, usize, usize) {
    graph
        .nodes()
        .iter()
        .fold((0, 0, 0), |(loops, decisions, switches), node| match node {
            Node::Loop(_) => (loops + 1, decisions, switches),
            Node::Decision(_) => (loops, decisions + 1, switches),
            Node::Switch(_) => (loops, decisions, switches + 1),
            _ => (loops, decisions, switches),
        })
}

#[test]
fn catalog_holds_three_approved_registrations_with_distinct_reasoning_shapes() {
    let key = contract_key();
    let contract_signature = GraphSignature::new()
        .require_output::<CodeCriterion>()
        .require_output::<CodeObservation>()
        .require_output::<Assessment<CodeCriterion>>()
        .require_output::<Vec<CodeCriterion>>()
        .require_output::<Vec<Assessment<CodeCriterion>>>()
        .produce::<AssessmentConclusion>();
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(
            TopologyContract::new(
                key.clone(),
                contract_signature,
                "produce an evidence-bearing assessment conclusion",
                "one immutable terminal conclusion",
            )
            .unwrap(),
        )
        .unwrap();

    let mut direct = Graph::new();
    direct.add_system(direct_conclusion);
    let coverage = coverage_graph();
    let correction = correction_graph();
    let shapes = [shape(&direct), shape(&coverage), shape(&correction)];
    assert_eq!(shapes, [(0, 0, 0), (1, 1, 0), (1, 1, 1)]);

    let registrations = [
        (
            "fixtures.direct",
            topology_descriptor("fixtures.direct", key.clone()),
            direct,
        ),
        (
            "fixtures.coverage",
            coverage_sweep::descriptor(key.clone(), qualification()).into_topology(),
            coverage,
        ),
        (
            "fixtures.correction",
            bounded_correction::descriptor(key, qualification()).into_topology(),
            correction,
        ),
    ];
    for (id, descriptor, graph) in registrations {
        catalog
            .register(
                RegistrationId::new(id).unwrap(),
                descriptor,
                ConfigurationDigest::new(format!("sha256:{id}")).unwrap(),
                graph,
                RegistrationProvenance::new("fixtures.cognition-pack").unwrap(),
            )
            .unwrap();
    }
    let snapshot = catalog.seal();
    assert_eq!(snapshot.candidates().len(), 3);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ResearchClaim {
    claim: String,
    status: Status,
}

async fn research_produce() -> ResearchClaim {
    ResearchClaim {
        claim: "intervention reduces measured outcome".to_owned(),
        status: Status::Pass,
    }
}

fn research_schema() -> SubjectSchema<ResearchClaim> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("fixtures.research-claim").unwrap(),
            Version::new(1, 0, 0),
        ),
        "A research claim and the expected citation judgement.",
        r#"{"type":"object","required":["claim","status"]}"#,
        |subject: &ResearchClaim| {
            if subject.claim.is_empty() {
                Err("claim must not be empty".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .unwrap()
}

#[system]
async fn research_judge(claim: Out<ResearchClaim>) -> Assessment<ResearchClaim> {
    Assessment::new(
        &research_schema(),
        claim.clone(),
        claim.status,
        vec![provenance("research-negative-control")],
    )
    .unwrap()
}

#[test]
fn coding_and_research_embodiments_share_blocks_without_sharing_domain_taxonomy() {
    let mut coding = Graph::new();
    approved::blocks::verified_step(
        &mut coding,
        erase(correction_produce),
        erase(correction_judge),
        &qualification(),
        approved::blocks::VerifiedRoles::Correction,
    );

    let mut research = Graph::new();
    approved::blocks::verified_step(
        &mut research,
        erase(research_produce),
        erase(research_judge),
        &qualification(),
        approved::blocks::VerifiedRoles::Correction,
    );

    assert!(coding.validate().is_ok());
    assert!(research.validate().is_ok());
    assert_eq!(shape(&coding), shape(&research));
    assert_ne!(
        std::any::TypeId::of::<CodeCriterion>(),
        std::any::TypeId::of::<ResearchClaim>()
    );
}

#[test]
fn public_builders_have_only_domain_parameters_after_typed_erasure() {
    fn accepts_coverage_builder(
        _builder: for<'g> fn(
            &'g mut Graph,
            coverage_sweep::Stages<CodeCriterion, CodeObservation>,
            OracleQualification,
        ) -> &'g mut Graph,
    ) {
    }

    fn accepts_correction_builder(
        _builder: for<'g> fn(
            &'g mut Graph,
            bounded_correction::Stages<CodeCriterion>,
            OracleQualification,
        ) -> &'g mut Graph,
    ) {
    }

    accepts_coverage_builder(coverage_sweep::build::<CodeCriterion, CodeObservation>);
    accepts_correction_builder(bounded_correction::build::<CodeCriterion>);
}

#[test]
fn approved_points_explain_every_boundary_they_introduce() {
    let subject = ContextTypeKey::new(
        ContextTypeId::new("fixtures.code-criterion").unwrap(),
        Version::new(1, 0, 0),
    );
    let mut registry = TypeExplanationRegistry::new();
    // Both points must agree on the keys they share, so registering the second
    // over the first is a rebinding conflict unless the derivation matches.
    coverage_sweep::register_type_explanations::<CodeCriterion>(&mut registry, &subject).unwrap();
    bounded_correction::register_type_explanations::<CodeCriterion>(&mut registry, &subject)
        .unwrap();

    for (point, graph) in [
        ("coverage_sweep", coverage_graph()),
        ("bounded_correction", correction_graph()),
    ] {
        let unexplained = StableSignatureExplanation::derive(&graph.signature(), &registry)
            .err()
            .unwrap_or_default();
        let cosmos_owned: Vec<&str> = unexplained
            .iter()
            .map(UnexplainedBoundary::display_type)
            .filter(|display| {
                display.contains("::approved::") || *display == std::any::type_name::<Status>()
            })
            .collect();
        assert!(
            cosmos_owned.is_empty(),
            "{point} leaves boundaries it introduces unexplained, \
             which excludes it from every selection: {cosmos_owned:?}"
        );
    }
}
