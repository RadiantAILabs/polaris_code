//! Validation and canonical-wire tests for the shared Layer-1 vocabulary.

use std::str::FromStr;

use polaris::system::plugin::Version;
use polaris_cosmos::contracts::{
    Assessment, CanonicalRecord, CardinalityEstimate, ConfidencePpm, ContentDigest, CurrencyCode,
    EpistemicBasis, EvidenceKind, EvidenceObjectId, EvidenceRef, EvidenceStoreId, EvidenceTime,
    FeatureAssessment, MeasurementId, Money, PreferenceBasisPoints, ProbabilityPpm, Provenance,
    SourceId, SourceKey, SourceKind, Status, SubjectSchema, SubjectSchemaId, SubjectSchemaKey,
    TokenAccountingId, TokenQuantity, UnknownReason,
};
use serde::{Deserialize, Serialize};

fn source() -> SourceKey {
    SourceKey::new(
        SourceId::new("tests.verifier").expect("source ID is valid"),
        Version::new(1, 0, 0),
    )
}

fn evidence(id: &str) -> EvidenceRef {
    EvidenceRef::new(
        EvidenceStoreId::new("tests.evidence").expect("store ID is valid"),
        EvidenceKind::VerifierOutput,
        EvidenceObjectId::new(id).expect("object ID is valid"),
    )
    .with_media_type("application/json")
    .expect("media type is valid")
    .with_byte_len(42)
}

fn provenance(id: &str) -> Provenance {
    Provenance::new(
        source(),
        SourceKind::AutomatedVerifier,
        EpistemicBasis::Verified,
    )
    .with_evidence(evidence(id))
}

fn u8_schema() -> SubjectSchema<u8> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("tests.counter").expect("schema ID is valid"),
            Version::new(1, 0, 0),
        ),
        "An unsigned test counter.",
        r#"{"type":"integer","minimum":0,"maximum":255}"#,
        |_| Ok(()),
    )
    .expect("schema binding is valid")
}

#[test]
fn validates_distinct_identity_and_quantity_classes() {
    assert!(SourceId::new("cosmos.policy/default").is_ok());
    assert!(SourceId::new("Cosmos.Policy").is_err());
    assert!(SourceId::new("").is_err());

    let event = "67e55044-10b1-426f-9247-bb680e5fe0c8"
        .parse::<polaris_cosmos::contracts::RunId>()
        .expect("lowercase UUIDv4 is valid");
    assert_eq!(event.to_string(), "67e55044-10b1-426f-9247-bb680e5fe0c8");
    assert!(
        "67E55044-10B1-426F-9247-BB680E5FE0C8"
            .parse::<polaris_cosmos::contracts::RunId>()
            .is_err()
    );
    assert!(
        "67e55044-10b1-326f-9247-bb680e5fe0c8"
            .parse::<polaris_cosmos::contracts::RunId>()
            .is_err()
    );

    assert_eq!(ConfidencePpm::new(1_000_000).unwrap().get(), 1_000_000);
    assert!(ProbabilityPpm::new(1_000_001).is_err());
    assert!(CardinalityEstimate::new(1, 4, 3).is_err());
    assert!(
        PreferenceBasisPoints::validate_total([
            PreferenceBasisPoints::new(4_000).unwrap(),
            PreferenceBasisPoints::new(6_000).unwrap(),
        ])
        .is_ok()
    );
    assert!(
        PreferenceBasisPoints::validate_total([
            PreferenceBasisPoints::new(4_000).unwrap(),
            PreferenceBasisPoints::new(5_999).unwrap(),
        ])
        .is_err()
    );

    let usd = CurrencyCode::from_str("USD").expect("currency is valid");
    let gbp = CurrencyCode::from_str("GBP").expect("currency is valid");
    assert_eq!(
        Money::new(2, usd)
            .checked_add(Money::new(3, usd))
            .unwrap()
            .micros(),
        5
    );
    assert!(Money::new(2, usd).checked_add(Money::new(3, gbp)).is_err());

    let scheme = TokenAccountingId::new("provider.tokens-v1").unwrap();
    let tokens = TokenQuantity::new(scheme.clone(), 10)
        .checked_add(&TokenQuantity::new(scheme, 5))
        .unwrap();
    assert_eq!(tokens.count(), 15);
}

#[test]
fn feature_assessment_never_fakes_unknown_confidence() {
    let attribution = vec![provenance("feature.known")];
    assert!(FeatureAssessment::known("value", ConfidencePpm::ZERO, attribution.clone(),).is_err());

    let known = FeatureAssessment::known(
        "value",
        ConfidencePpm::new(900_000).unwrap(),
        attribution.clone(),
    )
    .unwrap();
    assert_eq!(known.known_value(), Some(&"value"));
    assert_eq!(known.confidence().unwrap().get(), 900_000);
    assert_eq!(known.unknown_reason(), None);

    let unknown =
        FeatureAssessment::<String>::unknown(UnknownReason::ConflictingEvidence, attribution)
            .unwrap();
    assert!(unknown.known_value().is_none());
    assert!(unknown.confidence().is_none());
    assert_eq!(
        unknown.unknown_reason(),
        Some(UnknownReason::ConflictingEvidence)
    );
}

#[test]
fn canonical_assessment_round_trip_has_stable_golden_bytes_and_digest() {
    let assessment = Assessment::new(
        &u8_schema(),
        7,
        Status::Issue,
        vec![provenance("golden.verifier-output")],
    )
    .expect("assessment is valid");
    let bytes = assessment
        .to_canonical_cbor()
        .expect("assessment has a canonical wire form");
    let encoded = hex(&bytes);
    assert_eq!(
        encoded,
        "a666737461747573656973737565677375626a656374076a70726f76656e616e636581a665626173697368766572696669656466736f75726365a26269646e74657374732e76657269666965726776657273696f6ea3656d616a6f7201656d696e6f7200657061746368006b6f627365727665645f6174f66b736f757263655f6b696e64726175746f6d617465642d76657269666965726c646572697665645f66726f6d806d65766964656e63655f7265667381a662696476676f6c64656e2e76657269666965722d6f7574707574646b696e646f76657269666965722d6f75747075746573746f72656e74657374732e65766964656e636568627974655f6c656e182a6a6d656469615f74797065706170706c69636174696f6e2f6a736f6e6e636f6e74656e745f646967657374f66e636f6e74656e745f64696765737478477368613235363a626264396432346630636136653835383739363231353463616136633062343265393133373933363233353864616432633032386132653731383531323362666e736368656d615f76657273696f6e01727375626a6563745f736368656d615f6b6579a26269646d74657374732e636f756e7465726776657273696f6ea3656d616a6f7201656d696e6f720065706174636800"
    );
    assert_eq!(
        assessment.content_digest().to_string(),
        "sha256:bbd9d24f0ca6e8587962154caa6c0b42e91379362358dad2c028a2e7185123bf"
    );

    let decoded = Assessment::<u8>::from_canonical_cbor_with_schema(&bytes, &u8_schema())
        .expect("golden bytes decode under the bound subject schema");
    assert_eq!(decoded, assessment);
}

#[test]
fn informational_observation_time_does_not_change_assessment_content_identity() {
    let untimed = provenance("time.verifier-output");
    let timed = untimed
        .clone()
        .observed_at(EvidenceTime::new(None, Some(1_777_777_777)).unwrap());
    let first = Assessment::new(&u8_schema(), 7, Status::Issue, vec![untimed]).unwrap();
    let second = Assessment::new(&u8_schema(), 7, Status::Issue, vec![timed]).unwrap();

    assert_eq!(first.content_digest(), second.content_digest());
    assert_ne!(
        first.to_canonical_cbor().unwrap(),
        second.to_canonical_cbor().unwrap(),
        "informational event time remains inspectable in the complete record"
    );
}

#[test]
fn canonical_decode_rejects_noncanonical_or_tampered_records_and_closed_enums() {
    let assessment = Assessment::new(
        &u8_schema(),
        7,
        Status::Issue,
        vec![provenance("decode.verifier-output")],
    )
    .unwrap();
    let bytes = assessment.to_canonical_cbor().unwrap();

    let schema_key = b"schema_version";
    let schema_position =
        find_subslice(&bytes, schema_key).expect("schema field is encoded") + schema_key.len();
    assert_eq!(bytes[schema_position], 1);
    let mut noncanonical = bytes.clone();
    noncanonical.insert(schema_position, 0x18);
    assert_eq!(
        Assessment::<u8>::from_canonical_cbor(&noncanonical)
            .unwrap_err()
            .code(),
        "wire.non-canonical"
    );

    let mut tampered = bytes.clone();
    let digest_prefix = find_subslice(&tampered, b"sha256:").expect("digest is encoded");
    let digest_digit = digest_prefix + "sha256:".len();
    tampered[digest_digit] = if tampered[digest_digit] == b'0' {
        b'1'
    } else {
        b'0'
    };
    assert_eq!(
        Assessment::<u8>::from_canonical_cbor(&tampered)
            .unwrap_err()
            .code(),
        "identity.digest-mismatch"
    );

    let mut unknown_status = bytes;
    replace_same_length(&mut unknown_status, b"issue", b"bogus");
    assert_eq!(
        Assessment::<u8>::from_canonical_cbor(&unknown_status)
            .unwrap_err()
            .code(),
        "wire.decode"
    );
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FutureMeasurement {
    measurement: MeasurementId,
    value: u64,
}

#[test]
fn extensible_registry_names_survive_unknown_canonical_round_trips() {
    let schema = SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("tests.future-measurement").unwrap(),
            Version::new(1, 0, 0),
        ),
        "A future measurement registry entry.",
        r#"{"type":"object"}"#,
        |_: &FutureMeasurement| Ok(()),
    )
    .unwrap();
    let assessment = Assessment::new(
        &schema,
        FutureMeasurement {
            measurement: MeasurementId::new("future.quantum-quality").unwrap(),
            value: 9,
        },
        Status::Pass,
        vec![provenance("future.registry-evidence")],
    )
    .unwrap();
    let decoded = Assessment::<FutureMeasurement>::from_canonical_cbor_with_schema(
        &assessment.to_canonical_cbor().unwrap(),
        &schema,
    )
    .unwrap();
    assert_eq!(
        decoded.subject().measurement.as_str(),
        "future.quantum-quality"
    );
}

#[derive(Debug, Serialize)]
struct FloatSubject {
    score: f64,
}

#[test]
fn canonical_contracts_reject_float_subjects() {
    let schema = SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("tests.float").unwrap(),
            Version::new(1, 0, 0),
        ),
        "A deliberately invalid float subject.",
        r#"{"type":"number"}"#,
        |_: &FloatSubject| Ok(()),
    )
    .unwrap();
    assert_eq!(
        Assessment::new(
            &schema,
            FloatSubject { score: 0.5 },
            Status::Pass,
            vec![provenance("float.evidence")],
        )
        .unwrap_err()
        .code(),
        "wire.float-forbidden"
    );
}

#[test]
fn sha256_content_identity_uses_the_approved_spelling() {
    assert_eq!(
        ContentDigest::sha256(b"abc").to_string(),
        "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!("sha256:ABC".parse::<ContentDigest>().is_err());
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn replace_same_length(bytes: &mut [u8], old: &[u8], new: &[u8]) {
    assert_eq!(old.len(), new.len());
    let position = find_subslice(bytes, old).expect("old bytes are present");
    bytes[position..position + old.len()].copy_from_slice(new);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
