//! Evidence-bearing assessment envelope and immutable terminal accounting.

use std::collections::{BTreeMap, BTreeSet};
use std::marker::PhantomData;

use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::encoding::content_digest;
use super::evidence::{ProvenanceContent, canonical_provenance, insert_canonical};
use super::{
    CanonicalRecord, ContentDigest, ContractError, EvidenceRef, PolicyRef, Provenance,
    ResidualDisposition, SchemaVersion, Status, StopReason, SubjectSchemaKey,
};

/// Runtime binding between a concrete subject type and its stable wire schema.
///
/// Applications own the subject meaning and validator. Cosmos retains the
/// versioned key in canonical records and exposes both human-readable and
/// structured-generation schema text at registration time. This binding is a
/// process-local construction aid, not a second persisted record.
pub struct SubjectSchema<T> {
    key: SubjectSchemaKey,
    human_schema: String,
    structured_generation_schema: String,
    validate: fn(&T) -> Result<(), String>,
    subject: PhantomData<fn() -> T>,
}

impl<T> SubjectSchema<T> {
    /// Creates a complete schema binding for one concrete subject type.
    pub fn new(
        key: SubjectSchemaKey,
        human_schema: impl Into<String>,
        structured_generation_schema: impl Into<String>,
        validate: fn(&T) -> Result<(), String>,
    ) -> Result<Self, ContractError> {
        let human_schema = human_schema.into();
        let structured_generation_schema = structured_generation_schema.into();
        if human_schema.trim().is_empty() {
            return Err(ContractError::new(
                "assessment.empty-human-schema",
                "subject binding requires a human- and LLM-readable schema",
            ));
        }
        if structured_generation_schema.trim().is_empty() {
            return Err(ContractError::new(
                "assessment.empty-generation-schema",
                "subject binding requires a structured-generation schema",
            ));
        }
        Ok(Self {
            key,
            human_schema,
            structured_generation_schema,
            validate,
            subject: PhantomData,
        })
    }

    /// Returns the stable schema identity retained in assessment records.
    #[must_use]
    pub const fn key(&self) -> &SubjectSchemaKey {
        &self.key
    }

    /// Returns the human- and LLM-readable schema description.
    #[must_use]
    pub fn human_schema(&self) -> &str {
        &self.human_schema
    }

    /// Returns the application-supplied structured-generation schema.
    #[must_use]
    pub fn structured_generation_schema(&self) -> &str {
        &self.structured_generation_schema
    }

    /// Validates a concrete subject under this schema version.
    pub fn validate(&self, subject: &T) -> Result<(), ContractError> {
        (self.validate)(subject).map_err(|detail| {
            ContractError::new(
                "assessment.invalid-subject",
                format!("subject failed schema {}: {detail}", self.key),
            )
        })
    }
}

/// One immutable, attributable judgement about an application-owned subject.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assessment<T> {
    schema_version: SchemaVersion,
    subject_schema_key: SubjectSchemaKey,
    subject: T,
    status: Status,
    provenance: Vec<Provenance>,
    content_digest: ContentDigest,
}

impl<T: Serialize> Assessment<T> {
    /// Creates a validated assessment and computes its canonical content ID.
    pub fn new(
        subject_schema: &SubjectSchema<T>,
        subject: T,
        status: Status,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        subject_schema.validate(&subject)?;
        let provenance = canonical_provenance(provenance)?;
        if !provenance.iter().any(Provenance::has_evidence) {
            return Err(ContractError::new(
                "assessment.missing-evidence",
                "an assessment requires a typed evidence reference through provenance",
            ));
        }
        let mut assessment = Self {
            schema_version: SchemaVersion::V1,
            subject_schema_key: subject_schema.key().clone(),
            subject,
            status,
            provenance,
            content_digest: ContentDigest::from_sha256([0; 32]),
        };
        assessment.content_digest = assessment.expected_digest()?;
        Ok(assessment)
    }

    fn expected_digest(&self) -> Result<ContentDigest, ContractError> {
        #[derive(Serialize)]
        struct Body<'a, T> {
            schema_version: SchemaVersion,
            subject_schema_key: &'a SubjectSchemaKey,
            subject: &'a T,
            status: Status,
            provenance: Vec<ProvenanceContent<'a>>,
        }
        content_digest(&Body {
            schema_version: self.schema_version,
            subject_schema_key: &self.subject_schema_key,
            subject: &self.subject,
            status: self.status,
            provenance: self
                .provenance
                .iter()
                .map(Provenance::content_view)
                .collect(),
        })
    }

    /// Returns the record schema major.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns the stable schema bound to the concrete subject type.
    #[must_use]
    pub const fn subject_schema_key(&self) -> &SubjectSchemaKey {
        &self.subject_schema_key
    }

    /// Returns the complete application-owned subject.
    #[must_use]
    pub const fn subject(&self) -> &T {
        &self.subject
    }

    /// Returns the shared applicability and judgement status.
    #[must_use]
    pub const fn status(&self) -> Status {
        self.status
    }

    /// Returns the non-empty canonical provenance collection.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns the canonical content identity.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Revalidates the subject and stable key against a runtime schema binding.
    pub fn validate_with_schema(&self, schema: &SubjectSchema<T>) -> Result<(), ContractError> {
        self.validate_value()?;
        if &self.subject_schema_key != schema.key() {
            return Err(ContractError::new(
                "assessment.subject-schema-mismatch",
                format!(
                    "record uses {}, but the supplied binding is {}",
                    self.subject_schema_key,
                    schema.key()
                ),
            ));
        }
        schema.validate(&self.subject)
    }

    fn validate_value(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_canonical_set("assessment.provenance", &self.provenance, false)?;
        if !self.provenance.iter().any(Provenance::has_evidence) {
            return Err(ContractError::new(
                "assessment.missing-evidence",
                "an assessment requires a typed evidence reference through provenance",
            ));
        }
        require_digest(self.content_digest, self.expected_digest()?, "assessment")
    }
}

impl<T> Assessment<T>
where
    T: Serialize + DeserializeOwned,
{
    /// Decodes canonical CBOR and validates the concrete subject binding.
    pub fn from_canonical_cbor_with_schema(
        bytes: &[u8],
        schema: &SubjectSchema<T>,
    ) -> Result<Self, ContractError> {
        let assessment = <Self as CanonicalRecord>::from_canonical_cbor(bytes)?;
        assessment.validate_with_schema(schema)?;
        Ok(assessment)
    }
}

impl<T> CanonicalRecord for Assessment<T>
where
    T: Serialize + DeserializeOwned,
{
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_value()
    }
}

/// Four independently visible status counts in an assessment summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCounts {
    pass: u64,
    issue: u64,
    not_applicable: u64,
    indeterminate: u64,
}

impl StatusCounts {
    fn from_statuses(statuses: impl IntoIterator<Item = Status>) -> Result<Self, ContractError> {
        let mut counts = Self {
            pass: 0,
            issue: 0,
            not_applicable: 0,
            indeterminate: 0,
        };
        for status in statuses {
            let count = match status {
                Status::Pass => &mut counts.pass,
                Status::Issue => &mut counts.issue,
                Status::NotApplicable => &mut counts.not_applicable,
                Status::Indeterminate => &mut counts.indeterminate,
            };
            *count = count.checked_add(1).ok_or_else(|| {
                ContractError::new("assessment.count-overflow", "status count overflowed u64")
            })?;
        }
        Ok(counts)
    }

    /// Returns the number of passing assessments.
    #[must_use]
    pub const fn pass(self) -> u64 {
        self.pass
    }

    /// Returns the number of issue assessments.
    #[must_use]
    pub const fn issue(self) -> u64 {
        self.issue
    }

    /// Returns the number of not-applicable assessments.
    #[must_use]
    pub const fn not_applicable(self) -> u64 {
        self.not_applicable
    }

    /// Returns the number of indeterminate assessments.
    #[must_use]
    pub const fn indeterminate(self) -> u64 {
        self.indeterminate
    }

    fn total(self) -> Result<u64, ContractError> {
        self.pass
            .checked_add(self.issue)
            .and_then(|value| value.checked_add(self.not_applicable))
            .and_then(|value| value.checked_add(self.indeterminate))
            .ok_or_else(|| {
                ContractError::new("assessment.count-overflow", "status total overflowed u64")
            })
    }

    fn residual_count(self) -> Result<u64, ContractError> {
        self.issue.checked_add(self.indeterminate).ok_or_else(|| {
            ContractError::new("assessment.count-overflow", "residual count overflowed u64")
        })
    }
}

/// Non-generic immutable digest summary of one bounded assessment episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentSummary {
    schema_version: SchemaVersion,
    input_assessment_digests: Vec<ContentDigest>,
    counts_by_status: StatusCounts,
    evidence_refs: Vec<EvidenceRef>,
    combination_policy: PolicyRef,
    policy_configuration_digest: ContentDigest,
    combination_provenance: Vec<Provenance>,
    content_digest: ContentDigest,
}

impl AssessmentSummary {
    /// Summarizes a complete non-empty input set without copying its subjects.
    pub fn new<T: Serialize>(
        assessments: &[Assessment<T>],
        combination_policy: PolicyRef,
        policy_configuration_digest: ContentDigest,
        combination_provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        if assessments.is_empty() {
            return Err(ContractError::new(
                "assessment.empty-summary",
                "an assessment summary requires at least one atomic assessment",
            ));
        }
        for assessment in assessments {
            assessment.validate_value()?;
        }
        let combination_provenance = canonical_provenance(combination_provenance)?;
        let mut input_assessment_digests = Vec::with_capacity(assessments.len());
        let mut evidence_refs = Vec::new();
        for assessment in assessments {
            insert_canonical(&mut input_assessment_digests, assessment.content_digest());
            for provenance in assessment.provenance() {
                for reference in provenance
                    .evidence_refs()
                    .iter()
                    .chain(provenance.derived_from())
                {
                    insert_canonical(&mut evidence_refs, reference.clone());
                }
            }
        }
        if input_assessment_digests.len() != assessments.len() {
            return Err(ContractError::new(
                "assessment.duplicate-input",
                "each assessment content ID must appear exactly once",
            ));
        }
        let counts_by_status =
            StatusCounts::from_statuses(assessments.iter().map(Assessment::status))?;
        let mut summary = Self {
            schema_version: SchemaVersion::V1,
            input_assessment_digests,
            counts_by_status,
            evidence_refs,
            combination_policy,
            policy_configuration_digest,
            combination_provenance,
            content_digest: ContentDigest::from_sha256([0; 32]),
        };
        summary.content_digest = summary.expected_digest()?;
        Ok(summary)
    }

    fn expected_digest(&self) -> Result<ContentDigest, ContractError> {
        #[derive(Serialize)]
        struct Body<'a> {
            schema_version: SchemaVersion,
            input_assessment_digests: &'a [ContentDigest],
            counts_by_status: StatusCounts,
            evidence_refs: &'a [EvidenceRef],
            combination_policy: &'a PolicyRef,
            policy_configuration_digest: ContentDigest,
            combination_provenance: Vec<ProvenanceContent<'a>>,
        }
        content_digest(&Body {
            schema_version: self.schema_version,
            input_assessment_digests: &self.input_assessment_digests,
            counts_by_status: self.counts_by_status,
            evidence_refs: &self.evidence_refs,
            combination_policy: &self.combination_policy,
            policy_configuration_digest: self.policy_configuration_digest,
            combination_provenance: self
                .combination_provenance
                .iter()
                .map(Provenance::content_view)
                .collect(),
        })
    }

    /// Returns the record schema major.
    #[must_use]
    pub const fn schema_version(&self) -> SchemaVersion {
        self.schema_version
    }

    /// Returns every atomic assessment content ID exactly once.
    #[must_use]
    pub fn input_assessment_digests(&self) -> &[ContentDigest] {
        &self.input_assessment_digests
    }

    /// Returns the independently visible status counts.
    #[must_use]
    pub const fn counts_by_status(&self) -> StatusCounts {
        self.counts_by_status
    }

    /// Returns the canonical union of atomic evidence references.
    #[must_use]
    pub fn evidence_refs(&self) -> &[EvidenceRef] {
        &self.evidence_refs
    }

    /// Returns the application-owned combination policy identity.
    #[must_use]
    pub const fn combination_policy(&self) -> &PolicyRef {
        &self.combination_policy
    }

    /// Returns the complete resolved policy configuration identity.
    #[must_use]
    pub const fn policy_configuration_digest(&self) -> ContentDigest {
        self.policy_configuration_digest
    }

    /// Returns the attribution for combination.
    #[must_use]
    pub fn combination_provenance(&self) -> &[Provenance] {
        &self.combination_provenance
    }

    /// Returns the summary content identity.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_value(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_canonical_set(
            "assessment.input-digests",
            &self.input_assessment_digests,
            false,
        )?;
        validate_canonical_set("assessment.evidence-refs", &self.evidence_refs, true)?;
        validate_canonical_set(
            "assessment.combination-provenance",
            &self.combination_provenance,
            false,
        )?;
        let input_count = u64::try_from(self.input_assessment_digests.len()).map_err(|_| {
            ContractError::new(
                "assessment.count-range",
                "assessment input count exceeds u64",
            )
        })?;
        if self.counts_by_status.total()? != input_count {
            return Err(ContractError::new(
                "assessment.status-count-mismatch",
                "status counts must total the complete input digest collection",
            ));
        }
        require_digest(self.content_digest, self.expected_digest()?, "summary")
    }

    fn validate_against<T: Serialize>(
        &self,
        assessments: &[Assessment<T>],
    ) -> Result<(), ContractError> {
        self.validate_value()?;
        let expected = Self::new(
            assessments,
            self.combination_policy.clone(),
            self.policy_configuration_digest,
            self.combination_provenance.clone(),
        )?;
        if self != &expected {
            return Err(ContractError::new(
                "assessment.summary-input-mismatch",
                "summary does not match the supplied complete assessment set",
            ));
        }
        Ok(())
    }
}

impl CanonicalRecord for AssessmentSummary {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_value()
    }
}

/// Immutable terminal treatment of one issue or indeterminate assessment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentAccounting {
    schema_version: SchemaVersion,
    assessment_digest: ContentDigest,
    disposition: ResidualDisposition,
    resolution_policy: PolicyRef,
    resolution_configuration_digest: ContentDigest,
    provenance: Vec<Provenance>,
    content_digest: ContentDigest,
}

impl AssessmentAccounting {
    /// Creates one evidence-bearing terminal accounting entry.
    pub fn new(
        assessment_digest: ContentDigest,
        disposition: ResidualDisposition,
        resolution_policy: PolicyRef,
        resolution_configuration_digest: ContentDigest,
        provenance: Vec<Provenance>,
    ) -> Result<Self, ContractError> {
        let provenance = canonical_provenance(provenance)?;
        if !provenance.iter().any(Provenance::has_evidence) {
            return Err(ContractError::new(
                "assessment.accounting-missing-evidence",
                "subjective residual treatment requires typed decision evidence",
            ));
        }
        let mut accounting = Self {
            schema_version: SchemaVersion::V1,
            assessment_digest,
            disposition,
            resolution_policy,
            resolution_configuration_digest,
            provenance,
            content_digest: ContentDigest::from_sha256([0; 32]),
        };
        accounting.content_digest = accounting.expected_digest()?;
        Ok(accounting)
    }

    fn expected_digest(&self) -> Result<ContentDigest, ContractError> {
        #[derive(Serialize)]
        struct Body<'a> {
            schema_version: SchemaVersion,
            assessment_digest: ContentDigest,
            disposition: ResidualDisposition,
            resolution_policy: &'a PolicyRef,
            resolution_configuration_digest: ContentDigest,
            provenance: Vec<ProvenanceContent<'a>>,
        }
        content_digest(&Body {
            schema_version: self.schema_version,
            assessment_digest: self.assessment_digest,
            disposition: self.disposition,
            resolution_policy: &self.resolution_policy,
            resolution_configuration_digest: self.resolution_configuration_digest,
            provenance: self
                .provenance
                .iter()
                .map(Provenance::content_view)
                .collect(),
        })
    }

    /// Returns the accounted atomic assessment identity.
    #[must_use]
    pub const fn assessment_digest(&self) -> ContentDigest {
        self.assessment_digest
    }

    /// Returns the recorded treatment of the residual.
    #[must_use]
    pub const fn disposition(&self) -> ResidualDisposition {
        self.disposition
    }

    /// Returns the application-owned resolution policy identity.
    #[must_use]
    pub const fn resolution_policy(&self) -> &PolicyRef {
        &self.resolution_policy
    }

    /// Returns the resolved resolution-policy configuration identity.
    #[must_use]
    pub const fn resolution_configuration_digest(&self) -> ContentDigest {
        self.resolution_configuration_digest
    }

    /// Returns the evidence-bearing attribution for the treatment.
    #[must_use]
    pub fn provenance(&self) -> &[Provenance] {
        &self.provenance
    }

    /// Returns this accounting record's content identity.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    fn validate_value(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        validate_canonical_set("assessment.accounting-provenance", &self.provenance, false)?;
        if !self.provenance.iter().any(Provenance::has_evidence) {
            return Err(ContractError::new(
                "assessment.accounting-missing-evidence",
                "subjective residual treatment requires typed decision evidence",
            ));
        }
        require_digest(
            self.content_digest,
            self.expected_digest()?,
            "assessment accounting",
        )
    }
}

impl CanonicalRecord for AssessmentAccounting {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_value()
    }
}

/// Derived exhaustive view of a terminal assessment conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConclusionKind {
    /// Every accounted residual was resolved.
    Resolved,
    /// At least one residual remains and processing did not escalate out.
    Unresolved,
    /// At least one residual remains and processing escalated out.
    Escalated,
}

/// Immutable terminal conclusion over one non-generic assessment summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentConclusion {
    schema_version: SchemaVersion,
    summary: AssessmentSummary,
    accounting: Vec<AssessmentAccounting>,
    stop_reason: StopReason,
    content_digest: ContentDigest,
}

impl AssessmentConclusion {
    /// Creates a complete terminal conclusion and validates every input's accounting.
    pub fn new<T: Serialize>(
        summary: AssessmentSummary,
        mut accounting: Vec<AssessmentAccounting>,
        stop_reason: StopReason,
        assessments: &[Assessment<T>],
    ) -> Result<Self, ContractError> {
        summary.validate_value()?;
        for entry in &accounting {
            entry.validate_value()?;
        }
        accounting.sort_by_key(AssessmentAccounting::assessment_digest);
        if accounting
            .windows(2)
            .any(|pair| pair[0].assessment_digest() == pair[1].assessment_digest())
        {
            return Err(ContractError::new(
                "assessment.duplicate-accounting",
                "an assessment may have exactly one accounting entry",
            ));
        }
        let mut conclusion = Self {
            schema_version: SchemaVersion::V1,
            summary,
            accounting,
            stop_reason,
            content_digest: ContentDigest::from_sha256([0; 32]),
        };
        conclusion.validate_structure()?;
        conclusion.content_digest = conclusion.expected_digest()?;
        conclusion.validate_against(assessments)?;
        Ok(conclusion)
    }

    fn expected_digest(&self) -> Result<ContentDigest, ContractError> {
        #[derive(Serialize)]
        struct Body<'a> {
            schema_version: SchemaVersion,
            summary: &'a AssessmentSummary,
            accounting: &'a [AssessmentAccounting],
            stop_reason: StopReason,
        }
        content_digest(&Body {
            schema_version: self.schema_version,
            summary: &self.summary,
            accounting: &self.accounting,
            stop_reason: self.stop_reason,
        })
    }

    /// Returns the immutable non-generic summary.
    #[must_use]
    pub const fn summary(&self) -> &AssessmentSummary {
        &self.summary
    }

    /// Returns canonical terminal accounting entries.
    #[must_use]
    pub fn accounting(&self) -> &[AssessmentAccounting] {
        &self.accounting
    }

    /// Returns the semantic stopping reason.
    #[must_use]
    pub const fn stop_reason(&self) -> StopReason {
        self.stop_reason
    }

    /// Derives resolved, unresolved, or escalated without storing a duplicate fact.
    #[must_use]
    pub fn conclusion_kind(&self) -> ConclusionKind {
        let has_unresolved = self
            .accounting
            .iter()
            .any(|entry| entry.disposition() != ResidualDisposition::Resolved);
        match (has_unresolved, self.stop_reason) {
            (true, StopReason::EscalatedOut) => ConclusionKind::Escalated,
            (true, _) => ConclusionKind::Unresolved,
            (false, _) => ConclusionKind::Resolved,
        }
    }

    /// Returns the conclusion content identity.
    #[must_use]
    pub const fn content_digest(&self) -> ContentDigest {
        self.content_digest
    }

    /// Validates complete accounting against retained atomic assessments.
    pub fn validate_against<T: Serialize>(
        &self,
        assessments: &[Assessment<T>],
    ) -> Result<(), ContractError> {
        self.validate_value()?;
        self.summary.validate_against(assessments)?;

        let statuses: BTreeMap<ContentDigest, Status> = assessments
            .iter()
            .map(|assessment| (assessment.content_digest(), assessment.status()))
            .collect();
        let accounted: BTreeSet<ContentDigest> = self
            .accounting
            .iter()
            .map(AssessmentAccounting::assessment_digest)
            .collect();
        for (digest, status) in statuses {
            let must_account = matches!(status, Status::Issue | Status::Indeterminate);
            if accounted.contains(&digest) != must_account {
                return Err(ContractError::new(
                    "assessment.accounting-status-mismatch",
                    "issues and indeterminate assessments require exactly one entry; pass and not-applicable require none",
                ));
            }
        }
        Ok(())
    }

    /// Decodes canonical CBOR and validates it against retained atomic assessments.
    pub fn from_canonical_cbor_with_assessments<T: Serialize>(
        bytes: &[u8],
        assessments: &[Assessment<T>],
    ) -> Result<Self, ContractError> {
        let conclusion = <Self as CanonicalRecord>::from_canonical_cbor(bytes)?;
        conclusion.validate_against(assessments)?;
        Ok(conclusion)
    }

    fn validate_structure(&self) -> Result<(), ContractError> {
        let expected_accounting = self.summary.counts_by_status.residual_count()?;
        let actual_accounting = u64::try_from(self.accounting.len()).map_err(|_| {
            ContractError::new(
                "assessment.count-range",
                "assessment accounting count exceeds u64",
            )
        })?;
        if expected_accounting != actual_accounting {
            return Err(ContractError::new(
                "assessment.incomplete-accounting",
                "accounting count must equal issue plus indeterminate status counts",
            ));
        }
        let inputs: BTreeSet<_> = self
            .summary
            .input_assessment_digests
            .iter()
            .copied()
            .collect();
        if self
            .accounting
            .iter()
            .any(|entry| !inputs.contains(&entry.assessment_digest()))
        {
            return Err(ContractError::new(
                "assessment.accounting-unknown-input",
                "every accounting entry must name an input assessment",
            ));
        }
        let has_unresolved = self
            .accounting
            .iter()
            .any(|entry| entry.disposition() != ResidualDisposition::Resolved);
        if self.stop_reason == StopReason::EscalatedOut && !has_unresolved {
            return Err(ContractError::new(
                "assessment.escalation-invariant",
                "escalated-out requires at least one non-resolved residual",
            ));
        }
        Ok(())
    }

    fn validate_value(&self) -> Result<(), ContractError> {
        self.schema_version.require_v1()?;
        self.summary.validate_value()?;
        for entry in &self.accounting {
            entry.validate_value()?;
        }
        validate_sorted_accounting(&self.accounting)?;
        self.validate_structure()?;
        require_digest(
            self.content_digest,
            self.expected_digest()?,
            "assessment conclusion",
        )
    }
}

impl CanonicalRecord for AssessmentConclusion {
    fn validate(&self) -> Result<(), ContractError> {
        self.validate_value()
    }
}

fn validate_sorted_accounting(values: &[AssessmentAccounting]) -> Result<(), ContractError> {
    if values
        .windows(2)
        .any(|pair| pair[0].assessment_digest() >= pair[1].assessment_digest())
    {
        return Err(ContractError::new(
            "assessment.non-canonical-accounting",
            "accounting entries must be strictly ordered by assessment digest",
        ));
    }
    Ok(())
}

fn validate_canonical_set<T: Ord>(
    code: &'static str,
    values: &[T],
    allow_empty: bool,
) -> Result<(), ContractError> {
    if !allow_empty && values.is_empty() {
        return Err(ContractError::new(code, "canonical collection is empty"));
    }
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ContractError::new(
            code,
            "canonical collection must be strictly sorted and duplicate-free",
        ));
    }
    Ok(())
}

fn require_digest(
    actual: ContentDigest,
    expected: ContentDigest,
    record: &'static str,
) -> Result<(), ContractError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ContractError::new(
            "identity.digest-mismatch",
            format!("{record} digest {actual} does not match canonical content {expected}"),
        ))
    }
}
