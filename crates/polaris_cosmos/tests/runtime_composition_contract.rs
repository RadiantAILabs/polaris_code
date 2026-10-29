//! Contract proofs for finite runtime composition records.

use polaris::system::plugin::Version;
use polaris_cosmos::contracts::{
    AdmissionAttemptId, CanonicalRecord, CompositionBlockId, CompositionBlockKey,
    CompositionBlockRef, CompositionCatalogId, CompositionCatalogKey, CompositionChoice,
    CompositionChoiceOrigin, CompositionDecision, CompositionEvidence, CompositionOutcome,
    CompositionPlan, CompositionRecipeId, CompositionRecipeKey, CompositionRecipeRef,
    CompositionRun, CompositionValidation, CompositionValidationDisposition, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    EpistemicBasis, ExclusionCode, LogicalSequence, OutcomeObservationId, PolicyId, PolicyRef,
    Provenance, RecordedAt, ResolvedConfigurationRef, SelectionDecisionId, SourceId, SourceKey,
};
use polaris_cosmos::topology::{TopologyContractId, TopologyContractKey};

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn config(label: &str) -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("fixture.composition-config").unwrap(),
            version(),
        ),
        ConfigurationValue::map(vec![
            ConfigurationEntry::new("label", ConfigurationValue::String(label.into())).unwrap(),
        ])
        .unwrap(),
    )
    .unwrap()
}

fn plan(block_version: Version) -> CompositionPlan {
    CompositionPlan::new(
        CompositionCatalogKey::new(
            CompositionCatalogId::new("fixture.approved-compositions").unwrap(),
            version(),
        ),
        ContentDigest::sha256(b"sealed-composition-catalogue"),
        CompositionRecipeRef::new(
            CompositionRecipeKey::new(
                CompositionRecipeId::new("fixture.direct-verified").unwrap(),
                version(),
            ),
            config("recipe"),
        )
        .unwrap(),
        vec![
            CompositionBlockRef::new(
                CompositionBlockKey::new(
                    CompositionBlockId::new("fixture.verified-step").unwrap(),
                    block_version,
                ),
                config("block"),
            )
            .unwrap(),
        ],
    )
    .unwrap()
}

fn source() -> SourceKey {
    SourceKey::new(
        SourceId::new("fixture.composition-source").unwrap(),
        version(),
    )
}

fn provenance() -> Provenance {
    Provenance::new(
        source(),
        polaris_cosmos::contracts::SourceKind::CosmosPolicy,
        EpistemicBasis::Verified,
    )
}

fn policy() -> PolicyRef {
    PolicyRef::new(
        PolicyId::new("fixture.composition-policy").unwrap(),
        version(),
    )
}

fn decision(origin: CompositionChoiceOrigin) -> CompositionDecision {
    CompositionDecision::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c1"
            .parse::<SelectionDecisionId>()
            .unwrap(),
        RecordedAt::new(LogicalSequence::new(10), Some(1_000)),
        CompositionChoice::new(plan(version()), origin).unwrap(),
        vec![provenance()],
    )
    .unwrap()
}

fn evidence(origin: CompositionChoiceOrigin) -> CompositionEvidence {
    let decision = decision(origin);
    let validation = CompositionValidation::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c2"
            .parse::<AdmissionAttemptId>()
            .unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(11), Some(1_001)),
        polaris_cosmos::contracts::NativeGraphSignatureWitness::new(
            TopologyContractKey::new(
                TopologyContractId::new("fixture.result").unwrap(),
                version(),
            ),
            ContentDigest::sha256(b"native-derived-signature"),
        ),
        CompositionValidationDisposition::Admitted,
    )
    .unwrap();
    let run = CompositionRun::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c3".parse().unwrap(),
        &validation,
        RecordedAt::new(LogicalSequence::new(12), Some(1_002)),
    )
    .unwrap();
    let outcome = CompositionOutcome::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c4"
            .parse::<OutcomeObservationId>()
            .unwrap(),
        &run,
        RecordedAt::new(LogicalSequence::new(13), Some(1_003)),
    )
    .unwrap();
    CompositionEvidence::new(decision, validation, run, outcome).unwrap()
}

#[test]
fn composition_digest_is_canonical_and_component_versions_distinguish_plans() {
    let first = plan(version());
    let same = plan(version());
    let changed_block = plan(Version::new(1, 0, 1));

    assert_eq!(first.composition_digest(), same.composition_digest());
    assert_ne!(
        first.composition_digest(),
        changed_block.composition_digest()
    );
    assert_eq!(
        CompositionPlan::from_canonical_cbor(&first.to_canonical_cbor().unwrap()).unwrap(),
        first
    );
}

#[test]
fn invalid_plans_and_runs_after_refusal_are_rejected() {
    let recipe = CompositionRecipeRef::new(
        CompositionRecipeKey::new(
            CompositionRecipeId::new("fixture.direct-verified").unwrap(),
            version(),
        ),
        config("recipe"),
    )
    .unwrap();
    let empty = CompositionPlan::new(
        CompositionCatalogKey::new(
            CompositionCatalogId::new("fixture.approved-compositions").unwrap(),
            version(),
        ),
        ContentDigest::sha256(b"sealed-composition-catalogue"),
        recipe,
        vec![],
    )
    .unwrap_err();
    assert_eq!(empty.code(), "composition.invalid-block-count");

    let decision = decision(CompositionChoiceOrigin::Autonomous { policy: policy() });
    let refused = CompositionValidation::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c2"
            .parse::<AdmissionAttemptId>()
            .unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(11), None),
        polaris_cosmos::contracts::NativeGraphSignatureWitness::new(
            TopologyContractKey::new(
                TopologyContractId::new("fixture.result").unwrap(),
                version(),
            ),
            ContentDigest::sha256(b"native-derived-signature"),
        ),
        CompositionValidationDisposition::Refused {
            stage: ExclusionCode::new("native-validation").unwrap(),
            reason: ExclusionCode::new("graph.invalid").unwrap(),
        },
    )
    .unwrap();
    let error = CompositionRun::new(
        "67e55044-10b1-426f-8247-bb680e5fe0c3".parse().unwrap(),
        &refused,
        RecordedAt::new(LogicalSequence::new(12), None),
    )
    .unwrap_err();
    assert_eq!(error.code(), "composition.run-without-admission");
}

#[test]
fn ordered_evidence_preserves_registered_topology_ids() {
    let evidence = evidence(CompositionChoiceOrigin::Autonomous { policy: policy() });

    assert_eq!(
        evidence.decision().decision_id(),
        evidence.run().decision_id()
    );
    assert_eq!(evidence.run().run_id(), evidence.outcome().run_id());
    assert_eq!(
        evidence.decision().composition_digest(),
        evidence.outcome().composition_digest()
    );
    assert!(
        CompositionValidation::new(
            "67e55044-10b1-426f-8247-bb680e5fe0c2".parse().unwrap(),
            evidence.decision(),
            evidence.decision().recorded_at(),
            evidence.validation().native_signature().clone(),
            CompositionValidationDisposition::Admitted,
        )
        .is_err()
    );
}

#[test]
fn autonomous_and_forced_choices_remain_distinct_after_snapshot_reload() {
    let autonomous = evidence(CompositionChoiceOrigin::Autonomous { policy: policy() });
    let forced = evidence(CompositionChoiceOrigin::Forced { source: source() });

    let autonomous =
        CompositionEvidence::from_canonical_cbor(&autonomous.to_canonical_cbor().unwrap()).unwrap();
    let forced =
        CompositionEvidence::from_canonical_cbor(&forced.to_canonical_cbor().unwrap()).unwrap();
    assert!(matches!(
        autonomous.decision().choice().origin(),
        CompositionChoiceOrigin::Autonomous { .. }
    ));
    assert!(matches!(
        forced.decision().choice().origin(),
        CompositionChoiceOrigin::Forced { .. }
    ));
    assert_ne!(autonomous.content_digest(), forced.content_digest());
}
