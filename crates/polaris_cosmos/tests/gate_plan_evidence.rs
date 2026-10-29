//! Evidence proofs for planner-submitted gate plans and revision loops.

use polaris::system::plugin::Version;
use polaris_cosmos::composition::{
    BlockInstanceId, GateExpr, GatePath, GatePlan, Refusal, StablePortId, StablePortKey, StepPlan,
    WirePlan,
};
use polaris_cosmos::contracts::{
    CanonicalRecord, CompositionChoice, CompositionChoiceOrigin, CompositionDecision,
    CompositionOutcome, CompositionPlan, CompositionRecipeId, CompositionRecipeKey,
    CompositionRecipeRef, CompositionRevisionEvidence, CompositionRevisionTrail, CompositionRun,
    CompositionValidation, CompositionValidationDisposition, ConfigurationEntry,
    ConfigurationSchemaId, ConfigurationSchemaKey, ConfigurationValue, ContentDigest,
    EpistemicBasis, ExclusionCode, LogicalSequence, OutcomeObservationId, RecordedAt,
    ResolvedConfigurationRef, RevisionAttempt, SelectionDecisionId, SourceId, SourceKey,
};
use polaris_cosmos::evidence::{
    GatePlanOutcomeIndex, gate_expression_shape, gate_plan_record, gate_plan_shape,
};
use polaris_cosmos::topology::{TopologyContractId, TopologyContractKey};

#[path = "fixtures/composition.rs"]
#[allow(dead_code)]
mod fixtures;

/// Golden canonical bytes of a `CompositionOutcome` without gate-plan metadata.
const LEGACY_COMPOSITION_OUTCOME_CBOR: &[u8] = &[
    0xa7, 0x66, 0x72, 0x75, 0x6e, 0x5f, 0x69, 0x64, 0x78, 0x24, 0x36, 0x37, 0x65, 0x35, 0x35, 0x30,
    0x34, 0x34, 0x2d, 0x31, 0x30, 0x62, 0x31, 0x2d, 0x34, 0x32, 0x36, 0x66, 0x2d, 0x38, 0x32, 0x34,
    0x37, 0x2d, 0x62, 0x62, 0x36, 0x38, 0x30, 0x65, 0x35, 0x66, 0x65, 0x32, 0x31, 0x33, 0x6b, 0x64,
    0x65, 0x63, 0x69, 0x73, 0x69, 0x6f, 0x6e, 0x5f, 0x69, 0x64, 0x78, 0x24, 0x36, 0x37, 0x65, 0x35,
    0x35, 0x30, 0x34, 0x34, 0x2d, 0x31, 0x30, 0x62, 0x31, 0x2d, 0x34, 0x32, 0x36, 0x66, 0x2d, 0x38,
    0x32, 0x34, 0x37, 0x2d, 0x62, 0x62, 0x36, 0x38, 0x30, 0x65, 0x35, 0x66, 0x65, 0x32, 0x31, 0x31,
    0x6b, 0x72, 0x65, 0x63, 0x6f, 0x72, 0x64, 0x65, 0x64, 0x5f, 0x61, 0x74, 0xa2, 0x68, 0x73, 0x65,
    0x71, 0x75, 0x65, 0x6e, 0x63, 0x65, 0x17, 0x6d, 0x77, 0x61, 0x6c, 0x6c, 0x5f, 0x63, 0x6c, 0x6f,
    0x63, 0x6b, 0x5f, 0x6d, 0x73, 0xf6, 0x6e, 0x63, 0x6f, 0x6e, 0x74, 0x65, 0x6e, 0x74, 0x5f, 0x64,
    0x69, 0x67, 0x65, 0x73, 0x74, 0x78, 0x47, 0x73, 0x68, 0x61, 0x32, 0x35, 0x36, 0x3a, 0x38, 0x65,
    0x33, 0x36, 0x32, 0x36, 0x66, 0x36, 0x35, 0x61, 0x39, 0x38, 0x64, 0x33, 0x33, 0x66, 0x35, 0x33,
    0x63, 0x63, 0x33, 0x35, 0x37, 0x31, 0x35, 0x66, 0x38, 0x36, 0x66, 0x32, 0x64, 0x39, 0x64, 0x63,
    0x30, 0x64, 0x39, 0x64, 0x37, 0x39, 0x39, 0x32, 0x64, 0x37, 0x30, 0x65, 0x64, 0x39, 0x61, 0x31,
    0x65, 0x63, 0x65, 0x61, 0x31, 0x38, 0x35, 0x31, 0x62, 0x33, 0x39, 0x32, 0x63, 0x62, 0x6e, 0x6f,
    0x62, 0x73, 0x65, 0x72, 0x76, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x5f, 0x69, 0x64, 0x78, 0x24, 0x36,
    0x37, 0x65, 0x35, 0x35, 0x30, 0x34, 0x34, 0x2d, 0x31, 0x30, 0x62, 0x31, 0x2d, 0x34, 0x32, 0x36,
    0x66, 0x2d, 0x38, 0x32, 0x34, 0x37, 0x2d, 0x62, 0x62, 0x36, 0x38, 0x30, 0x65, 0x35, 0x66, 0x65,
    0x32, 0x31, 0x34, 0x6e, 0x73, 0x63, 0x68, 0x65, 0x6d, 0x61, 0x5f, 0x76, 0x65, 0x72, 0x73, 0x69,
    0x6f, 0x6e, 0x01, 0x72, 0x63, 0x6f, 0x6d, 0x70, 0x6f, 0x73, 0x69, 0x74, 0x69, 0x6f, 0x6e, 0x5f,
    0x64, 0x69, 0x67, 0x65, 0x73, 0x74, 0x78, 0x47, 0x73, 0x68, 0x61, 0x32, 0x35, 0x36, 0x3a, 0x38,
    0x31, 0x30, 0x30, 0x64, 0x31, 0x37, 0x32, 0x31, 0x38, 0x63, 0x65, 0x32, 0x34, 0x36, 0x39, 0x62,
    0x31, 0x63, 0x36, 0x64, 0x32, 0x36, 0x31, 0x31, 0x32, 0x61, 0x36, 0x39, 0x63, 0x30, 0x35, 0x62,
    0x62, 0x34, 0x33, 0x35, 0x61, 0x62, 0x65, 0x38, 0x33, 0x35, 0x65, 0x65, 0x61, 0x31, 0x62, 0x38,
    0x34, 0x39, 0x64, 0x65, 0x35, 0x30, 0x63, 0x30, 0x65, 0x36, 0x62, 0x34, 0x64, 0x61, 0x32,
];
const LEGACY_COMPOSITION_OUTCOME_DIGEST: &str =
    "sha256:8e3626f65a98d33f53cc35715f86f2d9dc0d9d7992d70ed9a1ecea1851b392cb";

fn version() -> Version {
    Version::new(1, 0, 0)
}

fn legacy_config() -> ResolvedConfigurationRef {
    ResolvedConfigurationRef::inline(
        ConfigurationSchemaKey::new(
            ConfigurationSchemaId::new("tests.gate-evidence-config").unwrap(),
            version(),
        ),
        ConfigurationValue::map(vec![
            ConfigurationEntry::new("label", ConfigurationValue::String("legacy".into())).unwrap(),
        ])
        .unwrap(),
    )
    .unwrap()
}

fn legacy_decision(id: &str, sequence: u64) -> CompositionDecision {
    let plan = CompositionPlan::new(
        polaris_cosmos::contracts::CompositionCatalogKey::new(
            polaris_cosmos::contracts::CompositionCatalogId::new("tests.gate-evidence").unwrap(),
            version(),
        ),
        ContentDigest::sha256(b"legacy-catalogue"),
        CompositionRecipeRef::new(
            CompositionRecipeKey::new(
                CompositionRecipeId::new("tests.gate-evidence-recipe").unwrap(),
                version(),
            ),
            legacy_config(),
        )
        .unwrap(),
        vec![
            polaris_cosmos::contracts::CompositionBlockRef::new(
                polaris_cosmos::contracts::CompositionBlockKey::new(
                    polaris_cosmos::contracts::CompositionBlockId::new("tests.gate-evidence-block")
                        .unwrap(),
                    version(),
                ),
                legacy_config(),
            )
            .unwrap(),
        ],
    )
    .unwrap();
    let source = SourceKey::new(
        SourceId::new("tests.gate-evidence-source").unwrap(),
        version(),
    );
    CompositionDecision::new(
        id.parse::<SelectionDecisionId>().unwrap(),
        RecordedAt::new(LogicalSequence::new(sequence), None),
        CompositionChoice::new(
            plan,
            CompositionChoiceOrigin::Forced {
                source: source.clone(),
            },
        )
        .unwrap(),
        vec![polaris_cosmos::contracts::Provenance::new(
            source,
            polaris_cosmos::contracts::SourceKind::HostObservation,
            EpistemicBasis::Verified,
        )],
    )
    .unwrap()
}

fn gate_plan(instance: &str, config: &str) -> GatePlan {
    let catalogue = fixtures::catalogue();
    let mut configurations = fixtures::configurations();
    let original = configurations
        .values()
        .next()
        .expect("fixture has one configuration")
        .clone();
    let config_id = polaris_cosmos::composition::ConfigId::new(config).unwrap();
    configurations.insert(config_id.clone(), original);
    GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::seed_contract_key(),
        GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
            polaris_cosmos::composition::BlockInstanceId::new(instance).unwrap(),
            fixtures::emit_positive_seed_key(),
            config_id,
        )),
        Vec::new(),
        configurations,
    )
    .unwrap()
}

fn wire_shape_plan(labels: [&str; 3], wires: Vec<WirePlan>) -> GatePlan {
    let catalogue = fixtures::catalogue();
    let configurations = fixtures::configurations();
    let config = configurations
        .keys()
        .next()
        .expect("fixture has one configuration")
        .clone();
    GatePlan::new(
        catalogue.catalogue().clone(),
        catalogue.digest(),
        fixtures::seed_contract_key(),
        GateExpr::Seq(
            labels
                .into_iter()
                .map(|label| {
                    GateExpr::Step(StepPlan::new(
                        BlockInstanceId::new(label).unwrap(),
                        fixtures::emit_positive_seed_key(),
                        config.clone(),
                    ))
                })
                .collect(),
        ),
        wires,
        configurations,
    )
    .unwrap()
}

fn wire(reader: &str, producer: &str, port: StablePortKey) -> WirePlan {
    WirePlan::new(
        BlockInstanceId::new(reader).unwrap(),
        BlockInstanceId::new(producer).unwrap(),
        port,
    )
}

fn witness() -> polaris_cosmos::contracts::NativeGraphSignatureWitness {
    polaris_cosmos::contracts::NativeGraphSignatureWitness::new(
        TopologyContractKey::new(
            TopologyContractId::new("tests.gate-evidence-result").unwrap(),
            version(),
        ),
        ContentDigest::sha256(b"signature"),
    )
}

#[test]
fn shape_ignores_plan_local_configuration_and_instance_labels() {
    let first = gate_plan("first", "tests.gate-evidence-config-a");
    let second = gate_plan("second", "tests.gate-evidence-config-b");

    assert_ne!(first.plan_digest(), second.plan_digest());
    assert_eq!(
        gate_expression_shape(first.expression()),
        gate_expression_shape(second.expression())
    );
}

#[test]
fn shape_retains_wiring_while_ignoring_equivalent_instance_labels() {
    let first = wire_shape_plan(
        ["reader", "producer", "other"],
        vec![wire("reader", "producer", fixtures::seed_port_key())],
    );
    let relabelled = wire_shape_plan(
        ["read", "produce", "unused"],
        vec![wire("read", "produce", fixtures::seed_port_key())],
    );
    let changed_endpoint = wire_shape_plan(
        ["reader", "producer", "other"],
        vec![wire("reader", "other", fixtures::seed_port_key())],
    );
    let changed_port = wire_shape_plan(
        ["reader", "producer", "other"],
        vec![wire(
            "reader",
            "producer",
            StablePortKey::new(
                StablePortId::new("tests.gate-evidence.other-port").unwrap(),
                version(),
            ),
        )],
    );
    let unwired = wire_shape_plan(["reader", "producer", "other"], Vec::new());

    assert_ne!(first.plan_digest(), relabelled.plan_digest());
    assert_eq!(gate_plan_record(&first).shape(), gate_plan_shape(&first));
    assert_eq!(gate_plan_shape(&first), gate_plan_shape(&relabelled));
    assert_ne!(gate_plan_shape(&first), gate_plan_shape(&changed_endpoint));
    assert_ne!(gate_plan_shape(&first), gate_plan_shape(&changed_port));
    assert_ne!(gate_plan_shape(&first), gate_plan_shape(&unwired));
}

#[test]
fn revision_trail_requires_a_later_admission_before_one_run_and_indexes_outcome() {
    let first_plan = gate_plan("first", "tests.gate-evidence-config-a");
    let second_plan = gate_plan("second", "tests.gate-evidence-config-b");
    let first_decision = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe201", 10)
        .with_gate_plan(
            gate_plan_record(&first_plan),
            RevisionAttempt::new(1).unwrap(),
        )
        .unwrap();
    let refused = CompositionValidation::refused(
        "67e55044-10b1-426f-8247-bb680e5fe202".parse().unwrap(),
        &first_decision,
        RecordedAt::new(LogicalSequence::new(11), None),
        witness(),
        ExclusionCode::new("native-validation").unwrap(),
        ExclusionCode::new("graph.invalid").unwrap(),
        vec![Refusal::new(
            "plan.bad-config",
            GatePath::root(),
            None,
            "a known configuration",
            "unknown configuration",
            "choose a registered configuration",
        )],
    )
    .unwrap();
    let first = CompositionRevisionEvidence::new(first_decision, refused).unwrap();
    assert_eq!(first.validation().refusals().len(), 1);
    assert_eq!(first.validation().refusals()[0].code(), "plan.bad-config");
    assert_eq!(first.validation().gate_plan(), first.decision().gate_plan());

    let second_decision = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe203", 12)
        .with_gate_plan(
            gate_plan_record(&second_plan),
            RevisionAttempt::new(2).unwrap(),
        )
        .unwrap();
    let admitted = CompositionValidation::new(
        "67e55044-10b1-426f-8247-bb680e5fe204".parse().unwrap(),
        &second_decision,
        RecordedAt::new(LogicalSequence::new(13), None),
        witness(),
        CompositionValidationDisposition::Admitted,
    )
    .unwrap();
    let second = CompositionRevisionEvidence::new(second_decision, admitted.clone()).unwrap();
    let run = CompositionRun::new(
        "67e55044-10b1-426f-8247-bb680e5fe205".parse().unwrap(),
        &admitted,
        RecordedAt::new(LogicalSequence::new(14), None),
    )
    .unwrap();
    let outcome = CompositionOutcome::new(
        "67e55044-10b1-426f-8247-bb680e5fe206"
            .parse::<OutcomeObservationId>()
            .unwrap(),
        &run,
        RecordedAt::new(LogicalSequence::new(15), None),
    )
    .unwrap();
    let trail =
        CompositionRevisionTrail::new(vec![first, second], Some(run), Some(outcome)).unwrap();

    let outcome = trail.outcome().unwrap();
    assert_eq!(
        outcome.gate_plan().unwrap().plan_digest(),
        second_plan.plan_digest()
    );
    assert_eq!(outcome.revision_attempt().unwrap().get(), 2);

    let mut index = GatePlanOutcomeIndex::default();
    index.insert(&trail).unwrap();
    assert_eq!(index.outcomes_by_plan(second_plan.plan_digest()).len(), 1);
    assert_eq!(
        index.outcomes_by_shape(gate_plan_shape(&second_plan)).len(),
        1
    );
}

#[test]
fn refused_gate_plan_validation_requires_actionable_refusals() {
    let plan = gate_plan("first", "tests.gate-evidence-config-a");
    let decision = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe221", 30)
        .with_gate_plan(gate_plan_record(&plan), RevisionAttempt::new(1).unwrap())
        .unwrap();
    let error = CompositionValidation::new(
        "67e55044-10b1-426f-8247-bb680e5fe222".parse().unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(31), None),
        witness(),
        CompositionValidationDisposition::Refused {
            stage: ExclusionCode::new("native-validation").unwrap(),
            reason: ExclusionCode::new("graph.invalid").unwrap(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "composition.gate-plan-refusal-missing");

    let refused = CompositionValidation::refused(
        "67e55044-10b1-426f-8247-bb680e5fe222".parse().unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(31), None),
        witness(),
        ExclusionCode::new("native-validation").unwrap(),
        ExclusionCode::new("graph.invalid").unwrap(),
        vec![Refusal::new(
            "plan.bad-config",
            GatePath::root(),
            None,
            "a known configuration",
            "unknown configuration",
            "choose a registered configuration",
        )],
    )
    .unwrap();
    let mut malformed = serde_json::to_value(refused).unwrap();
    malformed["refusals"] = serde_json::Value::Array(Vec::new());
    let malformed = serde_json::from_value(malformed).unwrap();
    let error = CompositionRevisionEvidence::new(decision, malformed).unwrap_err();
    assert_eq!(error.code(), "composition.gate-plan-refusal-missing");

    let legacy = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe223", 30);
    assert!(
        CompositionValidation::new(
            "67e55044-10b1-426f-8247-bb680e5fe224".parse().unwrap(),
            &legacy,
            RecordedAt::new(LogicalSequence::new(31), None),
            witness(),
            CompositionValidationDisposition::Refused {
                stage: ExclusionCode::new("native-validation").unwrap(),
                reason: ExclusionCode::new("graph.invalid").unwrap(),
            },
        )
        .is_ok()
    );
}

#[test]
fn revision_trails_must_begin_with_attempt_one() {
    let plan = gate_plan("first", "tests.gate-evidence-config-a");
    let decision = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe231", 40)
        .with_gate_plan(gate_plan_record(&plan), RevisionAttempt::new(2).unwrap())
        .unwrap();
    let validation = CompositionValidation::refused(
        "67e55044-10b1-426f-8247-bb680e5fe232".parse().unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(41), None),
        witness(),
        ExclusionCode::new("native-validation").unwrap(),
        ExclusionCode::new("graph.invalid").unwrap(),
        vec![Refusal::new(
            "plan.bad-config",
            GatePath::root(),
            None,
            "a known configuration",
            "unknown configuration",
            "choose a registered configuration",
        )],
    )
    .unwrap();
    let revision = CompositionRevisionEvidence::new(decision, validation).unwrap();
    let error = CompositionRevisionTrail::new(vec![revision], None, None).unwrap_err();
    assert_eq!(error.code(), "composition.revision-first-attempt");
}

#[test]
fn legacy_composition_records_decode_without_gate_plan_metadata() {
    let decision = legacy_decision("67e55044-10b1-426f-8247-bb680e5fe211", 20);
    let validation = CompositionValidation::new(
        "67e55044-10b1-426f-8247-bb680e5fe212".parse().unwrap(),
        &decision,
        RecordedAt::new(LogicalSequence::new(21), None),
        witness(),
        CompositionValidationDisposition::Admitted,
    )
    .unwrap();
    let run = CompositionRun::new(
        "67e55044-10b1-426f-8247-bb680e5fe213".parse().unwrap(),
        &validation,
        RecordedAt::new(LogicalSequence::new(22), None),
    )
    .unwrap();
    let outcome = CompositionOutcome::new(
        "67e55044-10b1-426f-8247-bb680e5fe214"
            .parse::<OutcomeObservationId>()
            .unwrap(),
        &run,
        RecordedAt::new(LogicalSequence::new(23), None),
    )
    .unwrap();

    let decision_json = serde_json::to_value(&decision).unwrap();
    let validation_json = serde_json::to_value(&validation).unwrap();
    let run_json = serde_json::to_value(&run).unwrap();
    let outcome_json = serde_json::to_value(&outcome).unwrap();
    for record in [&decision_json, &validation_json, &run_json, &outcome_json] {
        assert!(record.get("gate_plan").is_none());
        assert!(record.get("revision_attempt").is_none());
    }
    assert!(validation_json.get("refusals").is_none());

    assert_eq!(
        CompositionDecision::from_canonical_cbor(&decision.to_canonical_cbor().unwrap()).unwrap(),
        decision
    );
    assert_eq!(
        CompositionValidation::from_canonical_cbor(&validation.to_canonical_cbor().unwrap())
            .unwrap(),
        validation
    );
    assert_eq!(
        CompositionRun::from_canonical_cbor(&run.to_canonical_cbor().unwrap()).unwrap(),
        run
    );
    assert_eq!(
        CompositionOutcome::from_canonical_cbor(&outcome.to_canonical_cbor().unwrap()).unwrap(),
        outcome
    );

    let fixture = CompositionOutcome::from_canonical_cbor(LEGACY_COMPOSITION_OUTCOME_CBOR).unwrap();
    assert_eq!(fixture, outcome);
    assert_eq!(
        fixture.to_canonical_cbor().unwrap(),
        LEGACY_COMPOSITION_OUTCOME_CBOR
    );
    assert_eq!(
        fixture.content_digest().to_string(),
        LEGACY_COMPOSITION_OUTCOME_DIGEST
    );
}
