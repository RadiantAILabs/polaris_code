//! The coding task payload and its projection onto the Cosmos profile.
//!
//! [`CodingTask`] is `polaris_code`'s own type: one unit of coding work, held
//! as three things the accepting side can always state —
//!
//! - a **statement** of the problem in the requester's own words,
//! - a **place to work**, the sandbox root every action resolves through, and
//! - a **success check**, the way to tell afterwards whether the work landed.
//!
//! Everything beyond that is optional. A caller that has already worked out
//! the change says so with [`ExactEdit`]s; a caller that has only a problem
//! statement supplies none, and the accepting side is not asked to pre-compute
//! an edit it does not have. [`CodingTask::new`] keeps the narrow
//! one-file/one-find/one-replace form as the special case it always was.
//!
//! None of that crosses into Cosmos. What crosses is
//! [`CodingTask::profile_evidence`] — a closed-vocabulary
//! [`TaskProfileEvidence`] whose every field carries the provenance of how
//! this embodiment came to believe it.
//!
//! The distinction the profile keeps is *known versus unknown*, never
//! *known versus defaulted*: a feature this embodiment cannot observe is
//! reported [`UnknownReason::NotObserved`] with no invented confidence, so a
//! selector applying [`MissingPreferencePolicy::Pessimistic`] can see the gap
//! rather than score against a fabricated value. A unit of work that states
//! only a problem is therefore *more* unknown than a stated edit, and says so,
//! rather than borrowing the stated edit's certainty.

use std::fmt;
use std::path::{Path, PathBuf};

use polaris_cosmos::contracts::{
    CapabilityId, CardinalityEstimate, ConfidencePpm, ConsequenceLevel, ContractError,
    DecompositionShape, DurationMs, EpistemicBasis, EvidenceKind, FeatureAssessment,
    HardConstraints, MissingPreferencePolicy, NeedLevel, Objective, ObservedLimit,
    PreferenceBasisPoints, PreferenceWeights, RiskLevel, SourceKind, TaskHorizon, TaskId,
    TaskProfile, TaskProfileEvidence, TieBreakPolicy, UnknownReason, VerifierAvailability,
    VerifierCapability, VerifierProfile,
};
use polaris_cosmos::selection::transparent_profile::{self, ProfileInputs};

use super::edit::{StubSite, scan_stub_sites};
use super::payload::CodingQuestion;
use super::{TASK_NAMESPACE, behavior_version, evidenced, policy, provenance};
use crate::tools::CodeToolsConfig;

/// Confidence this embodiment reports for a feature it read directly off disk.
const OBSERVED_CONFIDENCE: u32 = 950_000;

/// Confidence for a feature re-derived from the payload's shape rather than
/// measured, such as how uncertain a unique-match string replace is.
const DERIVED_CONFIDENCE: u32 = 800_000;

/// How the requester wants the work done, before it becomes preference weights.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingIntent {
    /// Prefer the lowest-latency route that still satisfies the contract.
    Fast,
    /// Prefer the route most likely to be right, accepting more work.
    Careful,
    /// Prefer the route that audits the whole tree and cannot silently miss a
    /// site, accepting latency linear in the site count.
    Thorough,
    /// Answer a question about the tree and change nothing.
    ///
    /// Unlike the other three this is not a point on a speed/care dial: it
    /// forbids source mutation outright, so the edit-shaped routes are
    /// excluded on the hard-constraint channel rather than out-scored. A
    /// question routed under [`Fast`](Self::Fast) would still be answered by
    /// editing a file, which is the behaviour this variant exists to stop.
    Answer,
}

/// Why a coding task could not be constructed.
#[derive(Debug)]
#[non_exhaustive]
pub enum CodingTaskError {
    /// The working directory does not exist or is not a directory.
    MissingWorkingDir {
        /// The rejected path.
        path: PathBuf,
    },
    /// The task reference is not a valid Cosmos stable name.
    InvalidReference {
        /// The rejected reference.
        reference: String,
        /// The contract error explaining the rejection.
        source: ContractError,
    },
    /// The search and replacement text are identical, so the edit is a no-op.
    EmptyEdit,
    /// A unit of work was accepted with nothing said about the problem.
    EmptyStatement,
    /// A path — an edit target or a scope entry — was blank.
    EmptyPath,
    /// A success check names nothing: an empty program, verifier, or marker.
    EmptySuccessCheck,
    /// A Cosmos record rejected a derived profile value.
    Contract(ContractError),
    /// Cosmos's profile-aggregation policy rejected the supplied inputs.
    ///
    /// Carried as a string because `ContractError`'s constructor is private:
    /// a consumer can propagate one but cannot mint one, so a consumer-side
    /// failure cannot be reported in Cosmos's own error type.
    Aggregation(String),
}

impl fmt::Display for CodingTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingWorkingDir { path } => {
                write!(
                    formatter,
                    "working directory {} is not usable",
                    path.display()
                )
            }
            Self::InvalidReference { reference, source } => {
                write!(formatter, "task reference '{reference}': {source}")
            }
            Self::EmptyEdit => formatter.write_str("the search and replacement text are identical"),
            Self::EmptyStatement => {
                formatter.write_str("the unit of work states no problem to solve")
            }
            Self::EmptyPath => formatter.write_str("a path is blank"),
            Self::EmptySuccessCheck => {
                formatter.write_str("the success check names nothing to run")
            }
            Self::Contract(error) => error.fmt(formatter),
            Self::Aggregation(detail) => formatter.write_str(detail),
        }
    }
}

impl std::error::Error for CodingTaskError {}

impl From<ContractError> for CodingTaskError {
    fn from(error: ContractError) -> Self {
        Self::Contract(error)
    }
}

/// One exact-string replacement in one sandbox-relative file.
///
/// A caller that has already worked out the change states it as one of these.
/// A caller that has not states none, and the unit of work is judged by its
/// [`SuccessCheck`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExactEdit {
    file: String,
    find: String,
    replace: String,
}

impl ExactEdit {
    /// Captures one exact-string replacement.
    ///
    /// # Errors
    ///
    /// Returns an error when `file` is blank or when the search and
    /// replacement text are identical, which would make the edit a no-op.
    pub fn new(
        file: impl Into<String>,
        find: impl Into<String>,
        replace: impl Into<String>,
    ) -> Result<Self, CodingTaskError> {
        let file = file.into();
        if file.trim().is_empty() {
            return Err(CodingTaskError::EmptyPath);
        }
        let find = find.into();
        let replace = replace.into();
        if find == replace {
            return Err(CodingTaskError::EmptyEdit);
        }
        Ok(Self {
            file,
            find,
            replace,
        })
    }

    /// Returns the target file, relative to the sandbox root.
    #[must_use]
    pub fn file(&self) -> &str {
        &self.file
    }

    /// Returns the exact text this edit replaces.
    #[must_use]
    pub fn find(&self) -> &str {
        &self.find
    }

    /// Returns the replacement text.
    #[must_use]
    pub fn replace(&self) -> &str {
        &self.replace
    }
}

/// A program and its argument vector, run in the sandbox root.
///
/// The arguments are a vector, never a shell string: a problem statement or a
/// path with a space, a quote, or a `$` in it is data, and handing it to a
/// shell would make it code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuccessCommand {
    program: String,
    args: Vec<String>,
}

impl SuccessCommand {
    /// Captures the command that decides whether the work succeeded.
    ///
    /// # Errors
    ///
    /// Returns [`CodingTaskError::EmptySuccessCheck`] when `program` is blank.
    pub fn new(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, CodingTaskError> {
        let program = program.into();
        if program.trim().is_empty() {
            return Err(CodingTaskError::EmptySuccessCheck);
        }
        Ok(Self {
            program,
            args: args.into_iter().map(Into::into).collect(),
        })
    }

    /// Returns the program to run.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns the argument vector, without the program name.
    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

/// How the host decides, afterwards, whether a unit of work succeeded.
///
/// The three variants are the three honest answers to "who judges this?", and
/// the embodiment names no test framework and no benchmark convention in any
/// of them:
///
/// - [`SuccessCheck::StatedEdits`] — the caller stated the change, so landing
///   it *is* success. The optional marker adds the whole-tree residual scan
///   this embodiment has always run.
/// - [`SuccessCheck::Command`] — the caller supplied a program whose exit
///   status is the verdict. This is what a unit of work verified by a test
///   suite uses, without the crate knowing what a test suite is.
/// - [`SuccessCheck::Delegated`] — the caller judges it themselves, later.
///   The run reports [`Status::Indeterminate`] and defers, rather than
///   guessing.
///
/// [`Status::Indeterminate`]: polaris_cosmos::contracts::Status::Indeterminate
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SuccessCheck {
    /// Success is the stated edits landing, optionally plus a clean
    /// whole-tree scan for `marker`.
    StatedEdits {
        /// Text that identifies a site as still unimplemented, if the caller
        /// named one.
        marker: Option<String>,
    },
    /// Success is `command` exiting zero in the sandbox root.
    Command(SuccessCommand),
    /// Success is judged outside this embodiment, by the named verifier.
    Delegated {
        /// Host-meaningful name of whoever will judge the work.
        verifier: String,
    },
}

impl SuccessCheck {
    /// Judges only whether the stated edits landed.
    #[must_use]
    pub const fn stated_edits() -> Self {
        Self::StatedEdits { marker: None }
    }

    /// Judges the stated edits plus a whole-tree scan for `marker`.
    ///
    /// # Errors
    ///
    /// Returns [`CodingTaskError::EmptySuccessCheck`] when `marker` is empty:
    /// an empty marker matches every line, which is a scan of everything
    /// rather than a scan for anything.
    pub fn stated_edits_with_marker(marker: impl Into<String>) -> Result<Self, CodingTaskError> {
        let marker = marker.into();
        if marker.is_empty() {
            return Err(CodingTaskError::EmptySuccessCheck);
        }
        Ok(Self::StatedEdits {
            marker: Some(marker),
        })
    }

    /// Judges the work by a program's exit status.
    ///
    /// # Errors
    ///
    /// Returns [`CodingTaskError::EmptySuccessCheck`] when `program` is blank.
    pub fn command(
        program: impl Into<String>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Self, CodingTaskError> {
        Ok(Self::Command(SuccessCommand::new(program, args)?))
    }

    /// Hands the verdict to something outside this embodiment.
    ///
    /// # Errors
    ///
    /// Returns [`CodingTaskError::EmptySuccessCheck`] when `verifier` is
    /// blank: a deferred verdict with no one named to reach it is not a check.
    pub fn delegated(verifier: impl Into<String>) -> Result<Self, CodingTaskError> {
        let verifier = verifier.into();
        if verifier.trim().is_empty() {
            return Err(CodingTaskError::EmptySuccessCheck);
        }
        Ok(Self::Delegated { verifier })
    }

    /// Returns the stub marker this check scans for, if it scans at all.
    #[must_use]
    pub fn marker(&self) -> Option<&str> {
        match self {
            Self::StatedEdits { marker } => marker.as_deref(),
            Self::Command(_) | Self::Delegated { .. } => None,
        }
    }
}

/// One unit of coding work: a problem, a place to work, and a way to tell
/// whether it succeeded.
///
/// This is the embodiment's own payload, and it is deliberately the *smallest*
/// thing a real unit of work always has. A statement and a sandbox root and a
/// success check are always available to the accepting side. Stated edits and
/// a declared file scope are not, so they are optional: the accepting side is
/// never asked to pre-compute an edit it does not have.
///
/// The contracts above this need to know none of it.
#[derive(Debug, Clone)]
pub struct CodingTask {
    reference: String,
    working_dir: PathBuf,
    statement: String,
    edits: Vec<ExactEdit>,
    scope: Vec<String>,
    success: SuccessCheck,
    intent: CodingIntent,
    latency_budget: Option<DurationMs>,
    question: Option<String>,
}

impl CodingTask {
    /// Captures the narrow form: one file, one exact-string edit, one stub
    /// marker.
    ///
    /// This is [`from_statement`](Self::from_statement) with the edit already
    /// worked out and [`SuccessCheck::StatedEdits`] as the verdict, kept as
    /// its own constructor because it is the shape a terminal command can
    /// type in one line.
    ///
    /// # Errors
    ///
    /// Returns an error when `working_dir` is not an existing directory, when
    /// `reference` is not a valid Cosmos stable name, or when the edit would be
    /// a no-op.
    pub fn new(
        reference: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        file: impl Into<String>,
        find: impl Into<String>,
        replace: impl Into<String>,
        stub_marker: impl Into<String>,
        intent: CodingIntent,
    ) -> Result<Self, CodingTaskError> {
        let edit = ExactEdit::new(file, find, replace)?;
        let stub_marker = stub_marker.into();
        let success = if stub_marker.is_empty() {
            SuccessCheck::stated_edits()
        } else {
            SuccessCheck::stated_edits_with_marker(stub_marker)?
        };
        let statement = format!("apply the stated edit to {}", edit.file());
        Self::from_statement(reference, working_dir, statement, intent, success)
            .map(|task| task.with_edits(vec![edit]))
    }

    /// Captures a unit of work from a problem statement, with no edit
    /// pre-computed.
    ///
    /// This is the wide form. `statement` is the problem in the requester's
    /// own words and is carried verbatim — it is data, never interpolated
    /// into a command. Add what is known with
    /// [`with_edits`](Self::with_edits) and [`with_scope`](Self::with_scope);
    /// what is not known stays unknown, and the profile says so.
    ///
    /// # Errors
    ///
    /// Returns an error when `working_dir` is not an existing directory, when
    /// `reference` is not a valid Cosmos stable name, or when `statement` is
    /// blank.
    pub fn from_statement(
        reference: impl Into<String>,
        working_dir: impl Into<PathBuf>,
        statement: impl Into<String>,
        intent: CodingIntent,
        success: SuccessCheck,
    ) -> Result<Self, CodingTaskError> {
        let reference = reference.into();
        let working_dir = working_dir.into();
        let statement = statement.into();
        if !working_dir.is_dir() {
            return Err(CodingTaskError::MissingWorkingDir { path: working_dir });
        }
        if statement.trim().is_empty() {
            return Err(CodingTaskError::EmptyStatement);
        }
        // Fail here rather than at the first Cosmos record, so an invalid host
        // reference never reaches a profile.
        TaskId::new(TASK_NAMESPACE, reference.as_str()).map_err(|source| {
            CodingTaskError::InvalidReference {
                reference: reference.clone(),
                source,
            }
        })?;
        Ok(Self {
            reference,
            working_dir,
            statement,
            edits: Vec::new(),
            scope: Vec::new(),
            success,
            intent,
            latency_budget: None,
            question: None,
        })
    }

    /// States the exact edits the caller has already worked out.
    ///
    /// Zero is the normal case for a unit of work described only by its
    /// problem statement.
    #[must_use]
    pub fn with_edits(mut self, edits: Vec<ExactEdit>) -> Self {
        self.edits = edits;
        self
    }

    /// Declares the files the work is believed to touch.
    ///
    /// This is a *belief about scope*, not a permission list: sandboxing is
    /// still the working directory. Blank entries are dropped rather than
    /// inflating the work estimate.
    #[must_use]
    pub fn with_scope(mut self, scope: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.scope = scope
            .into_iter()
            .map(Into::into)
            .filter(|path| !path.trim().is_empty())
            .collect();
        self
    }

    /// Records a latency limit the request actually stated.
    ///
    /// Without this the profile reports the limit
    /// [`UnknownReason::NotObserved`] rather than assuming there is none.
    #[must_use]
    pub const fn with_latency_budget(mut self, budget: DurationMs) -> Self {
        self.latency_budget = Some(budget);
        self
    }

    /// Returns the host-namespaced task identity Cosmos records refer to.
    ///
    /// # Panics
    ///
    /// Never: the reference was validated in [`CodingTask::new`].
    #[must_use]
    pub fn task_id(&self) -> TaskId {
        TaskId::new(TASK_NAMESPACE, self.reference.as_str())
            .expect("task reference was validated at construction")
    }

    /// Returns the sandbox configuration every coding action resolves through.
    #[must_use]
    pub fn tools_config(&self) -> CodeToolsConfig {
        CodeToolsConfig::new(self.working_dir.clone())
    }

    /// Returns the sandbox root.
    #[must_use]
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Returns the problem, in the requester's own words.
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }

    /// Returns the exact edits the caller stated, which may be none.
    #[must_use]
    pub fn edits(&self) -> &[ExactEdit] {
        &self.edits
    }

    /// Returns the files the caller believes the work touches, which may be
    /// none.
    #[must_use]
    pub fn scope(&self) -> &[String] {
        &self.scope
    }

    /// Returns how success will be judged.
    #[must_use]
    pub const fn success(&self) -> &SuccessCheck {
        &self.success
    }

    /// Returns the latency limit the requester explicitly declared, if any.
    ///
    /// `None` is unknown rather than an unlimited budget; policy callers must
    /// preserve that distinction when deciding a composition.
    #[must_use]
    pub const fn latency_budget(&self) -> Option<DurationMs> {
        self.latency_budget
    }

    /// Returns the marker that identifies an unimplemented site, when the
    /// success check scans for one.
    #[must_use]
    pub fn stub_marker(&self) -> Option<&str> {
        self.success.marker()
    }

    /// Returns the requester's intent.
    #[must_use]
    pub const fn intent(&self) -> CodingIntent {
        self.intent
    }

    /// Whether this task is a read-only survey: a declared scope to look at,
    /// a marker to look for, and no edit to apply.
    ///
    /// Distinct from [`CodingIntent::Answer`]: a survey states no question,
    /// only where to look and what counts as a finding — a caller can
    /// combine the two (a scope-and-marker survey that also asks a question
    /// about what it finds), so this is a fact about the task's own shape,
    /// never a synonym for the intent.
    #[must_use]
    pub fn is_survey(&self) -> bool {
        self.edits.is_empty() && !self.scope.is_empty() && self.stub_marker().is_some()
    }

    /// Records the question this request is actually asking.
    ///
    /// Held as the raw text rather than a parsed [`CodingQuestion`] so the
    /// refusal can quote the requester verbatim: recognition happens once, in
    /// [`question`](Self::question), where it can be inspected.
    #[must_use]
    pub fn with_question(mut self, text: impl Into<String>) -> Self {
        self.question = Some(text.into());
        self
    }

    /// Returns the question the answer topology will be given.
    ///
    /// A task that asked nothing yields [`CodingQuestion::none`], which is
    /// unanswerable — so a host that routes to the answer shape without
    /// setting a question gets an honest refusal rather than an empty pass.
    #[must_use]
    pub fn question(&self) -> CodingQuestion {
        self.question
            .as_deref()
            .map_or_else(CodingQuestion::none, CodingQuestion::parse)
    }

    /// Scans the working tree for the stub sites that remain right now.
    ///
    /// Empty when the success check names no marker: a scan for nothing finds
    /// nothing, rather than matching every line.
    #[must_use]
    pub fn stub_sites(&self) -> Vec<StubSite> {
        self.stub_marker()
            .map(|marker| scan_stub_sites(&self.working_dir, marker))
            .unwrap_or_default()
    }

    /// Returns how many independent parts this unit of work has, and where
    /// that number came from.
    ///
    /// The order is strongest evidence first: a real scan of the tree beats
    /// the caller's declared scope, which beats counting the stated edits.
    /// A unit of work that states only a problem yields
    /// [`WorkScale::Unknown`], which the profile reports as a gap rather than
    /// as "one part".
    fn work_scale(&self) -> WorkScale {
        if self.stub_marker().is_some() {
            let sites = self.stub_sites();
            return WorkScale::Scanned(u64::try_from(sites.len()).unwrap_or(u64::MAX));
        }
        if !self.scope.is_empty() {
            return WorkScale::Declared(u64::try_from(self.scope.len()).unwrap_or(u64::MAX));
        }
        let files: std::collections::BTreeSet<&str> =
            self.edits.iter().map(ExactEdit::file).collect();
        if files.is_empty() {
            WorkScale::Unknown
        } else {
            WorkScale::Declared(u64::try_from(files.len()).unwrap_or(u64::MAX))
        }
    }

    /// Projects the coding payload and the real repository onto the closed
    /// Cosmos feature vocabulary.
    ///
    /// Every field is attributed. Features read off disk are
    /// [`SourceKind::EmbodimentExtractor`] with [`EpistemicBasis::Verified`];
    /// features inferred from the request's shape are the same source with
    /// [`EpistemicBasis::ReDerived`]; the human-approval requirement is a
    /// [`SourceKind::HostObservation`] because it is a fact about this host's
    /// tool permissions, not about the task.
    ///
    /// # Errors
    ///
    /// Returns an error if a derived value is rejected by its Cosmos record —
    /// for example a verifier profile that claims availability with no
    /// capability.
    pub fn profile_evidence(&self) -> Result<TaskProfileEvidence, CodingTaskError> {
        let scale = self.work_scale();
        let observed = ConfidencePpm::new(OBSERVED_CONFIDENCE)?;
        let derived = ConfidencePpm::new(DERIVED_CONFIDENCE)?;

        let scanned = || {
            vec![evidenced(
                SourceKind::EmbodimentExtractor,
                EpistemicBasis::Verified,
                EvidenceKind::ArtifactDiff,
                "profile.stub-scan",
            )]
        };
        let inferred = || {
            vec![provenance(
                SourceKind::EmbodimentExtractor,
                EpistemicBasis::ReDerived,
            )]
        };
        let requested = || {
            vec![provenance(
                SourceKind::RequestDeclaration,
                EpistemicBasis::Asserted,
            )]
        };
        let host = || {
            vec![provenance(
                SourceKind::HostObservation,
                EpistemicBasis::Verified,
            )]
        };
        let unobserved = || {
            vec![provenance(
                SourceKind::EmbodimentExtractor,
                EpistemicBasis::Verified,
            )]
        };

        // One part is an indivisible edit; several are independent parts that
        // happen to share a unit of work. Where the count comes from — a real
        // scan, the caller's declaration, or nowhere — is what `scale` carries.
        let (decomposability, work, horizon) = match scale {
            WorkScale::Unknown => (
                FeatureAssessment::unknown(UnknownReason::NotObserved, unobserved())?,
                FeatureAssessment::unknown(UnknownReason::NotObserved, unobserved())?,
                FeatureAssessment::unknown(UnknownReason::NotObserved, unobserved())?,
            ),
            WorkScale::Scanned(count) | WorkScale::Declared(count) => {
                let parts = usize::try_from(count).unwrap_or(usize::MAX);
                let shape = if parts <= 1 {
                    DecompositionShape::Indivisible
                } else {
                    DecompositionShape::IndependentParts
                };
                let span = if parts <= 1 {
                    TaskHorizon::SingleStep
                } else {
                    TaskHorizon::ShortSequence
                };
                // A scan is verified off the tree; a declaration is the
                // caller's assertion, and is attributed as one.
                let attribution = || match scale {
                    WorkScale::Scanned(_) => scanned(),
                    WorkScale::Declared(_) | WorkScale::Unknown => requested(),
                };
                (
                    FeatureAssessment::known(shape, observed, attribution())?,
                    FeatureAssessment::known(
                        CardinalityEstimate::new(count, count, count)?,
                        observed,
                        attribution(),
                    )?,
                    FeatureAssessment::known(span, observed, attribution())?,
                )
            }
        };

        // A stated edit and an exit status are both mechanically checkable, so
        // this embodiment really does ship a deterministic verifier and no
        // comparative or heuristic one. A delegated verdict is somebody else's
        // verifier, whose capabilities this embodiment has not observed.
        let verifier = match self.success {
            SuccessCheck::StatedEdits { .. } | SuccessCheck::Command(_) => {
                FeatureAssessment::known(
                    VerifierProfile::new(
                        VerifierAvailability::Available,
                        vec![VerifierCapability::Deterministic],
                    )?,
                    observed,
                    host(),
                )?
            }
            SuccessCheck::Delegated { .. } => {
                FeatureAssessment::unknown(UnknownReason::NotObserved, unobserved())?
            }
        };

        // A unique-match exact-string replace has a mechanically checkable
        // precondition, so residual uncertainty is low. Nothing stated is the
        // opposite: the change itself still has to be found.
        let (uncertainty, exploration) = if self.edits.is_empty() {
            (NeedLevel::High, NeedLevel::High)
        } else if self.edits.iter().any(|edit| edit.find().is_empty()) {
            (NeedLevel::Low, NeedLevel::Medium)
        } else {
            (NeedLevel::Low, NeedLevel::None)
        };
        let refinement = match self.intent {
            CodingIntent::Fast => NeedLevel::Low,
            CodingIntent::Careful => NeedLevel::Medium,
            CodingIntent::Thorough => NeedLevel::High,
            // An answer is read once and either grounded or not; there is
            // nothing to refine between iterations.
            CodingIntent::Answer => NeedLevel::None,
        };

        let latency_limit = match self.latency_budget {
            Some(budget) => {
                FeatureAssessment::known(ObservedLimit::AtMost(budget), observed, requested())?
            }
            // No latency budget was stated for this task, so none was
            // observed. `NotStated` would be a stronger claim than this
            // embodiment can make.
            None => FeatureAssessment::unknown(UnknownReason::NotObserved, unobserved())?,
        };

        Ok(TaskProfileEvidence {
            decomposability: vec![decomposability],
            expected_independent_work: vec![work],
            verifier: vec![verifier],
            uncertainty: vec![FeatureAssessment::known(uncertainty, derived, inferred())?],
            exploration_need: vec![FeatureAssessment::known(exploration, derived, inferred())?],
            refinement_need: vec![FeatureAssessment::known(refinement, derived, requested())?],
            // One sandboxed working directory per run; nothing else is visible.
            context_isolation_need: vec![FeatureAssessment::known(
                NeedLevel::Low,
                observed,
                host(),
            )?],
            // `edit_file` is registered `ToolPermission::Confirm` in this host,
            // so a human really is in the write path.
            human_interaction_need: vec![FeatureAssessment::known(
                NeedLevel::Medium,
                observed,
                host(),
            )?],
            consequence: vec![FeatureAssessment::known(
                ConsequenceLevel::Moderate,
                derived,
                inferred(),
            )?],
            inherent_task_risk: vec![FeatureAssessment::known(
                RiskLevel::Low,
                derived,
                inferred(),
            )?],
            horizon: vec![horizon],
            requested_latency_limit: vec![latency_limit],
            // The task payload carries no token or monetary limit. Run caps
            // are enforced by the token meter, not recorded on the task.
            requested_token_limit: vec![FeatureAssessment::unknown(
                UnknownReason::NotObserved,
                unobserved(),
            )?],
            requested_monetary_limit: vec![FeatureAssessment::unknown(
                UnknownReason::NotObserved,
                unobserved(),
            )?],
        })
    }

    /// Aggregates the coding evidence into a frozen [`TaskProfile`].
    ///
    /// Aggregation itself is Cosmos's
    /// [`transparent_profile`]
    /// policy point, not this embodiment's: `polaris_code` supplies evidence
    /// and attribution and lets the shared policy fold it.
    ///
    /// # Errors
    ///
    /// Returns an error if evidence derivation fails or the aggregation policy
    /// rejects the inputs.
    pub fn profile(&self) -> Result<TaskProfile, CodingTaskError> {
        let evidence = self.profile_evidence()?;
        let inputs = ProfileInputs::new(
            self.task_id(),
            None,
            evidence,
            behavior_version(),
            vec![provenance(
                SourceKind::CosmosPolicy,
                EpistemicBasis::Asserted,
            )],
        )
        .map_err(|error| CodingTaskError::Aggregation(error.to_string()))?;
        transparent_profile::aggregate(&inputs)
            .map_err(|error| CodingTaskError::Aggregation(error.to_string()))
    }
}

/// How many independent parts a unit of work has, and on what evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkScale {
    /// Measured off the real tree by a stub scan.
    Scanned(u64),
    /// Asserted by the caller, as a declared scope or as stated edits.
    Declared(u64),
    /// Nothing said and nothing measurable: a genuine gap.
    Unknown,
}

/// The coding objective, expressed in Cosmos preference weights.
///
/// The two intents differ only in how they weight quality against latency.
/// Both keep the same hard constraints, because a coding edit is only ever
/// admissible with a deterministic verifier behind it.
#[derive(Debug, Clone, Copy)]
pub struct CodingObjective {
    intent: CodingIntent,
    require_comparative_verifier: bool,
}

impl CodingObjective {
    /// Creates the objective for one intent.
    #[must_use]
    pub const fn new(intent: CodingIntent) -> Self {
        Self {
            intent,
            require_comparative_verifier: false,
        }
    }

    /// Demands a verifier that can *compare* candidate results.
    ///
    /// No coding topology in this embodiment offers that, so this is how the
    /// host asks for something it cannot supply and gets a typed
    /// `NoEligibleCandidate` decision instead of a wrong answer.
    #[must_use]
    pub const fn requiring_comparative_verifier(mut self) -> Self {
        self.require_comparative_verifier = true;
        self
    }

    /// Builds the frozen Cosmos objective.
    ///
    /// # Errors
    ///
    /// Returns an error if the weight vector does not total 10,000 basis
    /// points or the constraints are inconsistent.
    pub fn build(self) -> Result<Objective, ContractError> {
        // (quality, verified_coverage, latency, execution_failure_risk)
        let (quality, coverage, latency, failure) = match self.intent {
            CodingIntent::Fast => (2_000, 0, 8_000, 0),
            CodingIntent::Careful => (8_000, 0, 2_000, 0),
            CodingIntent::Thorough => (0, 6_000, 0, 4_000),
            // Answering is mostly a latency question once the shapes that
            // must not run have been excluded outright below.
            CodingIntent::Answer => (3_000, 0, 7_000, 0),
        };
        let weights = PreferenceWeights::new(
            PreferenceBasisPoints::new(quality)?,
            PreferenceBasisPoints::new(coverage)?,
            PreferenceBasisPoints::new(latency)?,
            PreferenceBasisPoints::new(0)?,
            PreferenceBasisPoints::new(0)?,
            PreferenceBasisPoints::new(failure)?,
            PreferenceBasisPoints::new(0)?,
            PreferenceBasisPoints::new(0)?,
        )?;
        let mut hard = HardConstraints::none().with_required_verifier_capabilities(
            if self.require_comparative_verifier {
                vec![VerifierCapability::Comparative]
            } else {
                vec![VerifierCapability::Deterministic]
            },
        );
        if self.intent == CodingIntent::Answer {
            // The point of the answer intent. Without this a question is
            // decided on points, and the fastest way to "resolve" a question
            // scores as well as reading the tree.
            hard = hard.with_forbidden_capabilities(vec![CapabilityId::new(
                super::policy::SOURCE_MUTATION,
            )?]);
        }
        Objective::new(
            hard,
            weights,
            // A candidate with no evidence for a weighted criterion must not
            // out-rank one that has evidence, so missing evidence scores worst.
            MissingPreferencePolicy::Pessimistic,
            TieBreakPolicy::AscendingRegistrationId,
            vec![
                provenance(SourceKind::RequestDeclaration, EpistemicBasis::Asserted),
                provenance(SourceKind::PolicyDefault, EpistemicBasis::Asserted),
            ],
        )
    }

    /// Returns the named policy this objective was derived under.
    #[must_use]
    pub fn derivation_policy() -> polaris_cosmos::contracts::PolicyRef {
        policy("polaris-code.objective-derivation")
    }
}
