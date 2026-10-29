//! Coding embodiment of the `polaris_cosmos` topology-runtime contracts.
//!
//! [`polaris_cosmos`] owns the *domain-neutral* half of topology selection: a
//! catalog of executable graphs, a mechanical admission boundary, selection
//! policy points, an append-only evidence contract, and the shared value
//! vocabulary those records are written in. It deliberately owns nothing about
//! what a task *is*.
//!
//! This module supplies the missing half for coding work, and nothing more:
//!
//! | Cosmos contract | What `polaris_code` supplies here |
//! |---|---|
//! | [`TaskProfileEvidence`](polaris_cosmos::contracts::TaskProfileEvidence) | [`task::CodingTask::profile_evidence`] reads the real repository and reports closed-vocabulary features with per-field provenance. |
//! | [`Objective`](polaris_cosmos::contracts::Objective) | [`task::CodingObjective`] turns a coding intent ("fast" vs "careful") into preference weights and hard constraints. |
//! | [`TopologyCatalog`](polaris_cosmos::topology::TopologyCatalog) | [`topology::CodingTopologyPackPlugin`] contributes seven complete coding registrations through the ordinary plugin extension point, and [`default_loop::DefaultLoopTopologyPackPlugin`] publishes the default conversational loop as an eighth, under its own contract. |
//! | [`RuntimeAvailabilitySnapshot`](polaris_cosmos::contracts::RuntimeAvailabilitySnapshot) | [`topology::probe_availability`] observes the actual host: the workspace exists, a test runner does not. |
//! | [`TypeExplanationRegistry`](polaris_cosmos::topology::TypeExplanationRegistry) | The coding pack binds embodiment types and reuses both Cosmos explanation helpers while constructing complete registrations. |
//! | Execution | [`proof::CodingProof`] owns live context preparation and calls [`GraphExecutor`](polaris::graph::GraphExecutor) itself. Cosmos never executes. |
//! | Live host loop | [`host::select_and_run`] runs that same ordered lifecycle from the run surface's selection point, once per accepted coding task. |
//! | [`OutcomeObservation`](polaris_cosmos::contracts::OutcomeObservation) | [`edit`] runs real deterministic verifiers over the edited tree and projects their verdicts. |
//!
//! Nothing in this module changes a Cosmos type or reaches into a Cosmos
//! internal, and no coding semantics move upstream: the coding payload, the
//! sandbox, the editor, the verifiers, and graph execution all stay here.
//!
//! See [`proof`] for the offline end-to-end driver, [`host`] for the live one
//! wired into [`crate::run::drive_turn`], and `docs/cosmos-contracts.md` for the
//! verified consumer report.

pub mod assess;
pub mod block_cards;
pub mod compare;
pub mod composition;
pub mod default_loop;
pub mod domains;
pub mod edit;
pub mod gate_blocks;
pub mod graphs;
pub mod host;
pub mod inventory;
pub mod payload;
pub mod plan_history;
pub mod planner;
pub mod policy;
pub mod proof;
pub mod task;
pub mod topology;
pub mod verify;

use polaris::system::plugin::Version;
use polaris_cosmos::contracts::{
    EpistemicBasis, EvidenceKind, EvidenceObjectId, EvidenceRef, EvidenceStoreId, PolicyId,
    PolicyRef, Provenance, SourceId, SourceKey, SourceKind,
};

pub use compare::{ArmOutcome, ComparisonArm, ComparisonReport, compare_intents};
pub use edit::{EditError, EditOutcome, StubSite};
pub use host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TaskOrigin, TopologySelectionError,
    TopologyTurn, auto_route,
};
pub use inventory::{Blocker, BlockerCategory, TopologyEntry, TopologyInventory};
pub use payload::{CodingWorkspace, EditSite};
pub use plan_history::{PLAN_HISTORY_VAR, render_plan_history};
pub use proof::{CodingProof, CodingProofReport, ProofError, ProofEventIds};
pub use task::{
    CodingIntent, CodingObjective, CodingTask, CodingTaskError, ExactEdit, SuccessCheck,
    SuccessCommand,
};

/// Stable source identity of this embodiment, used for every `polaris_code`
/// attribution written into a Cosmos record.
pub const EMBODIMENT_SOURCE: &str = "polaris-code.coding-embodiment";

/// Namespace `polaris_code` allocates its host references
/// ([`TaskId`](polaris_cosmos::contracts::TaskId)) under.
pub const TASK_NAMESPACE: &str = "polaris-code";

/// Evidence store the embodiment names when it links a verifier artifact.
pub const EVIDENCE_STORE: &str = "polaris-code.coding-evidence";

/// Behaviour version of every policy and source key this module allocates.
///
/// One version covers the whole embodiment on purpose: profiling, objective
/// derivation, and outcome projection are revised together, and Cosmos
/// requires policy provenance to attest to the exact behaviour version it
/// aggregated under.
#[must_use]
pub const fn behavior_version() -> Version {
    Version::new(1, 0, 0)
}

/// Returns the embodiment's versioned source key.
///
/// # Panics
///
/// Never: [`EMBODIMENT_SOURCE`] is a compile-time constant that satisfies the
/// Cosmos stable-name grammar, and the assertion is covered by
/// `tests/cosmos_contracts_proof.rs`.
#[must_use]
pub fn source_key() -> SourceKey {
    SourceKey::new(
        SourceId::new(EMBODIMENT_SOURCE).expect("embodiment source ID is a valid stable name"),
        behavior_version(),
    )
}

/// Builds attribution for one fact this embodiment reports.
///
/// `kind` records *who* observed the fact and `basis` records *how strongly*:
/// a feature read off disk is [`EpistemicBasis::Verified`], a value the user
/// merely asserted in the request is [`EpistemicBasis::Asserted`].
#[must_use]
pub fn provenance(kind: SourceKind, basis: EpistemicBasis) -> Provenance {
    Provenance::new(source_key(), kind, basis)
}

/// Builds attribution that links the artifact a verifier actually produced.
///
/// # Panics
///
/// Panics if `object` is not a valid Cosmos stable name. Callers in this crate
/// pass constants.
#[must_use]
pub fn evidenced(
    kind: SourceKind,
    basis: EpistemicBasis,
    evidence_kind: EvidenceKind,
    object: &str,
) -> Provenance {
    provenance(kind, basis).with_evidence(EvidenceRef::new(
        EvidenceStoreId::new(EVIDENCE_STORE).expect("evidence store ID is a valid stable name"),
        evidence_kind,
        EvidenceObjectId::new(object).expect("evidence object ID is a valid stable name"),
    ))
}

/// Returns a versioned reference to one of this embodiment's named policies.
///
/// # Panics
///
/// Panics if `id` is not a valid Cosmos stable name. Callers in this crate
/// pass constants.
#[must_use]
pub fn policy(id: &str) -> PolicyRef {
    PolicyRef::new(
        PolicyId::new(id).expect("policy ID is a valid stable name"),
        behavior_version(),
    )
}
