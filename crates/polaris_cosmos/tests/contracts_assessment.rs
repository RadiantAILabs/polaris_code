//! Cross-domain examples and terminal-invariant tests for `Assessment<T>`.

use polaris::system::plugin::Version;
use polaris_cosmos::contracts::{
    Assessment, AssessmentAccounting, AssessmentConclusion, AssessmentSummary, CanonicalRecord,
    ConclusionKind, ContentDigest, EpistemicBasis, EvidenceKind, EvidenceObjectId, EvidenceRef,
    EvidenceStoreId, PolicyId, PolicyRef, Provenance, ResidualDisposition, SourceId, SourceKey,
    SourceKind, Status, StopReason, SubjectSchema, SubjectSchemaId, SubjectSchemaKey,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CodeCriterion {
    change_ref: String,
    criterion: String,
    repository_policy_version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ClaimCriterion {
    claim_ref: String,
    criterion: String,
    population: String,
    ontology_version: u64,
}

fn code_schema() -> SubjectSchema<CodeCriterion> {
    SubjectSchema::new(
        schema_key("examples.code-criterion"),
        "A code change and the repository criterion being judged.",
        r#"{"type":"object","required":["change_ref","criterion","repository_policy_version"]}"#,
        |subject: &CodeCriterion| {
            if subject.change_ref.is_empty() || subject.criterion.is_empty() {
                Err("change_ref and criterion must be non-empty".into())
            } else {
                Ok(())
            }
        },
    )
    .unwrap()
}

fn claim_schema() -> SubjectSchema<ClaimCriterion> {
    SubjectSchema::new(
        schema_key("examples.claim-criterion"),
        "A research claim, criterion, population, and ontology version.",
        r#"{"type":"object","required":["claim_ref","criterion","population","ontology_version"]}"#,
        |subject: &ClaimCriterion| {
            if subject.claim_ref.is_empty()
                || subject.criterion.is_empty()
                || subject.population.is_empty()
            {
                Err("claim_ref, criterion, and population must be non-empty".into())
            } else {
                Ok(())
            }
        },
    )
    .unwrap()
}

fn schema_key(id: &str) -> SubjectSchemaKey {
    SubjectSchemaKey::new(
        SubjectSchemaId::new(id).expect("schema ID is valid"),
        Version::new(1, 0, 0),
    )
}

fn policy(id: &str) -> PolicyRef {
    PolicyRef::new(
        PolicyId::new(id).expect("policy ID is valid"),
        Version::new(1, 0, 0),
    )
}

fn provenance(id: &str, kind: SourceKind) -> Provenance {
    Provenance::new(
        SourceKey::new(
            SourceId::new("examples.verifier").unwrap(),
            Version::new(1, 0, 0),
        ),
        kind,
        EpistemicBasis::Verified,
    )
    .with_evidence(EvidenceRef::new(
        EvidenceStoreId::new("examples.evidence").unwrap(),
        EvidenceKind::VerifierOutput,
        EvidenceObjectId::new(id).expect("evidence ID is valid"),
    ))
}

fn configuration(id: &str) -> ContentDigest {
    ContentDigest::sha256(id.as_bytes())
}

fn accounting(
    assessment: ContentDigest,
    disposition: ResidualDisposition,
    evidence_id: &str,
) -> AssessmentAccounting {
    AssessmentAccounting::new(
        assessment,
        disposition,
        policy("examples.resolve"),
        configuration("resolve-config-v1"),
        vec![provenance(evidence_id, SourceKind::HumanReview)],
    )
    .unwrap()
}

#[test]
fn coding_and_research_use_one_outer_vocabulary_without_shared_domain_taxonomy() {
    let code_assessments = vec![
        Assessment::new(
            &code_schema(),
            CodeCriterion {
                change_ref: "change:auth-hardening".into(),
                criterion: "repository-policy:authentication-regression".into(),
                repository_policy_version: 3,
            },
            Status::Issue,
            vec![provenance(
                "coding.auth-diff",
                SourceKind::AutomatedVerifier,
            )],
        )
        .unwrap(),
        Assessment::new(
            &code_schema(),
            CodeCriterion {
                change_ref: "change:auth-hardening".into(),
                criterion: "repository-policy:flaky-integration-test".into(),
                repository_policy_version: 3,
            },
            Status::Indeterminate,
            vec![provenance(
                "coding.flaky-test",
                SourceKind::AutomatedVerifier,
            )],
        )
        .unwrap(),
    ];
    let code_summary = AssessmentSummary::new(
        &code_assessments,
        policy("examples.combine-code"),
        configuration("code-policy-v1"),
        vec![provenance("coding.combination", SourceKind::CosmosPolicy)],
    )
    .unwrap();
    let code_conclusion = AssessmentConclusion::new(
        code_summary,
        vec![
            accounting(
                code_assessments[0].content_digest(),
                ResidualDisposition::Resolved,
                "coding.auth-resolution",
            ),
            accounting(
                code_assessments[1].content_digest(),
                ResidualDisposition::Deferred,
                "coding.flaky-deferral",
            ),
        ],
        StopReason::WorkExhausted,
        &code_assessments,
    )
    .unwrap();
    code_conclusion.validate_against(&code_assessments).unwrap();
    assert_eq!(
        code_conclusion.conclusion_kind(),
        ConclusionKind::Unresolved
    );

    let research_assessments = vec![
        Assessment::new(
            &claim_schema(),
            ClaimCriterion {
                claim_ref: "claim:urban-heat-mortality".into(),
                criterion: "causal-identification:confounders".into(),
                population: "population:uk-cities".into(),
                ontology_version: 7,
            },
            Status::Issue,
            vec![provenance(
                "research.contradictory-study",
                SourceKind::HumanReview,
            )],
        )
        .unwrap(),
    ];
    let research_summary = AssessmentSummary::new(
        &research_assessments,
        policy("examples.combine-research"),
        configuration("research-policy-v1"),
        vec![provenance("research.combination", SourceKind::CosmosPolicy)],
    )
    .unwrap();
    let research_conclusion = AssessmentConclusion::new(
        research_summary,
        vec![accounting(
            research_assessments[0].content_digest(),
            ResidualDisposition::Blocked,
            "research.investigator-handoff",
        )],
        StopReason::EscalatedOut,
        &research_assessments,
    )
    .unwrap();
    research_conclusion
        .validate_against(&research_assessments)
        .unwrap();
    assert_eq!(
        research_conclusion.conclusion_kind(),
        ConclusionKind::Escalated
    );

    let wire = research_conclusion.to_canonical_cbor().unwrap();
    assert_eq!(
        AssessmentConclusion::from_canonical_cbor_with_assessments(&wire, &research_assessments)
            .unwrap(),
        research_conclusion
    );
}

#[test]
fn conclusion_kind_is_derived_from_accounting_and_stop_reason() {
    let assessments = vec![
        Assessment::new(
            &code_schema(),
            CodeCriterion {
                change_ref: "change:one".into(),
                criterion: "criterion:one".into(),
                repository_policy_version: 1,
            },
            Status::Issue,
            vec![provenance("kind.issue", SourceKind::AutomatedVerifier)],
        )
        .unwrap(),
    ];
    let summary = AssessmentSummary::new(
        &assessments,
        policy("examples.combine"),
        configuration("combine-v1"),
        vec![provenance("kind.combine", SourceKind::CosmosPolicy)],
    )
    .unwrap();

    let resolved = AssessmentConclusion::new(
        summary.clone(),
        vec![accounting(
            assessments[0].content_digest(),
            ResidualDisposition::Resolved,
            "kind.resolved",
        )],
        StopReason::GoalMet,
        &assessments,
    )
    .unwrap();
    assert_eq!(resolved.conclusion_kind(), ConclusionKind::Resolved);

    let unresolved = AssessmentConclusion::new(
        summary.clone(),
        vec![accounting(
            assessments[0].content_digest(),
            ResidualDisposition::Parked,
            "kind.parked",
        )],
        StopReason::WorkExhausted,
        &assessments,
    )
    .unwrap();
    assert_eq!(unresolved.conclusion_kind(), ConclusionKind::Unresolved);

    let escalated = AssessmentConclusion::new(
        summary,
        vec![accounting(
            assessments[0].content_digest(),
            ResidualDisposition::Blocked,
            "kind.escalated",
        )],
        StopReason::EscalatedOut,
        &assessments,
    )
    .unwrap();
    assert_eq!(escalated.conclusion_kind(), ConclusionKind::Escalated);
}

#[test]
fn terminal_invariants_reject_missing_wrong_or_false_escalation_accounting() {
    let assessments = vec![
        Assessment::new(
            &code_schema(),
            CodeCriterion {
                change_ref: "change:two".into(),
                criterion: "criterion:pass".into(),
                repository_policy_version: 1,
            },
            Status::Pass,
            vec![provenance("invariant.pass", SourceKind::AutomatedVerifier)],
        )
        .unwrap(),
        Assessment::new(
            &code_schema(),
            CodeCriterion {
                change_ref: "change:two".into(),
                criterion: "criterion:issue".into(),
                repository_policy_version: 1,
            },
            Status::Issue,
            vec![provenance("invariant.issue", SourceKind::AutomatedVerifier)],
        )
        .unwrap(),
    ];
    let summary = AssessmentSummary::new(
        &assessments,
        policy("examples.invariant-combine"),
        configuration("invariant-combine-v1"),
        vec![provenance("invariant.combine", SourceKind::CosmosPolicy)],
    )
    .unwrap();

    assert_eq!(
        AssessmentConclusion::new(
            summary.clone(),
            vec![],
            StopReason::WorkExhausted,
            &assessments,
        )
        .unwrap_err()
        .code(),
        "assessment.incomplete-accounting"
    );

    assert_eq!(
        AssessmentConclusion::new(
            summary.clone(),
            vec![accounting(
                assessments[0].content_digest(),
                ResidualDisposition::Resolved,
                "invariant.wrong-target",
            )],
            StopReason::GoalMet,
            &assessments,
        )
        .unwrap_err()
        .code(),
        "assessment.accounting-status-mismatch"
    );

    assert_eq!(
        AssessmentConclusion::new(
            summary,
            vec![accounting(
                assessments[1].content_digest(),
                ResidualDisposition::Resolved,
                "invariant.false-escalation",
            )],
            StopReason::EscalatedOut,
            &assessments,
        )
        .unwrap_err()
        .code(),
        "assessment.escalation-invariant"
    );
}

#[test]
fn subject_schema_binding_rejects_invalid_or_mismatched_domain_values() {
    let invalid = CodeCriterion {
        change_ref: String::new(),
        criterion: "criterion:one".into(),
        repository_policy_version: 1,
    };
    assert_eq!(
        Assessment::new(
            &code_schema(),
            invalid,
            Status::Issue,
            vec![provenance("schema.invalid", SourceKind::AutomatedVerifier)],
        )
        .unwrap_err()
        .code(),
        "assessment.invalid-subject"
    );

    let assessment = Assessment::new(
        &code_schema(),
        CodeCriterion {
            change_ref: "change:three".into(),
            criterion: "criterion:one".into(),
            repository_policy_version: 1,
        },
        Status::Pass,
        vec![provenance("schema.valid", SourceKind::AutomatedVerifier)],
    )
    .unwrap();
    let other_schema = SubjectSchema::new(
        schema_key("examples.other-code-criterion"),
        "A deliberately different schema binding.",
        r#"{"type":"object"}"#,
        |_: &CodeCriterion| Ok(()),
    )
    .unwrap();
    assert_eq!(
        assessment
            .validate_with_schema(&other_schema)
            .unwrap_err()
            .code(),
        "assessment.subject-schema-mismatch"
    );
}
