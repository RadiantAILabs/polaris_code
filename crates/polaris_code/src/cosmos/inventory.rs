//! What this build's sealed topology catalog holds, and whether each
//! registration can run here.
//!
//! Registration is compile-time and the catalog is sealed at `ready`. That
//! makes "which topologies does this build have" a question with one true
//! answer, but until now the only place that answer was written down was the
//! private pack function that registers them. This module derives the answer
//! instead of restating it:
//!
//! - **what exists** comes from [`TopologyCatalogSnapshot::candidates`], so a
//!   new registration appears here without any edit to this file;
//! - **what each one is for** comes from the registration's own
//!   [`TopologyDescriptor`](polaris_cosmos::topology::TopologyDescriptor) —
//!   nothing here invents a description a registration does not carry; and
//! - **whether it can run here** comes from running the same
//!   [`preflight_catalog`] over the same
//!   [`RuntimeAvailabilitySnapshot`](polaris_cosmos::contracts::RuntimeAvailabilitySnapshot)
//!   the live selection point uses, so the listed verdict is the verdict.
//!
//! Everything here is read-only. Building an inventory registers nothing,
//! executes nothing, and leaves the catalog and the session untouched: the
//! snapshot is taken by shared reference, the availability probe performs one
//! directory `stat`, and preflight is a pure function.
//!
//! An excluded registration carries Cosmos's stable exclusion codes.
//! [`BlockerCategory`] is this host's own reading of those codes — the four
//! situations a user has to tell apart — and it is derived from the code, not
//! attached to a registration by hand.

use std::collections::BTreeMap;
use std::path::Path;

use polaris_cosmos::contracts::{
    AdmissionResult, AvailabilitySnapshotId, ContentDigest, ContractError, ExclusionReason,
    RuntimeAvailabilitySnapshot,
};
use polaris_cosmos::topology::{
    AdmissionBuildError, TopologyCandidate, TopologyCatalogSnapshot, TopologyContractKey,
    preflight_catalog,
};
use serde::Serialize;

use super::topology::{self, HostCapabilities};

/// The kind of situation that stops a registration from running here.
///
/// Cosmos's exclusion codes are extensible and deliberately fine-grained. A
/// user reading a list needs the coarser question answered first: is this a
/// broken build, an unwired host, something nobody looked at, or simply a
/// registration that answers a different question? Those four are the
/// categories, and every known code maps to exactly one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockerCategory {
    /// The registration itself, or the catalog it came from, is wrong. This
    /// build cannot run it and no host configuration will change that.
    Defect,
    /// Nothing observed the capability the registration needs, so admission
    /// could not decide either way.
    Unobserved,
    /// The registration is sound; this host has not wired what it needs.
    /// Wiring the capability makes it runnable without rebuilding.
    HostUnwired,
    /// The registration is fine and runnable in its own terms; it just does
    /// not claim the contract that was asked about.
    NotApplicable,
    /// The exclusion code is newer than this host's reading of the vocabulary.
    Unrecognized,
}

impl BlockerCategory {
    /// Reads one stable Cosmos exclusion code as one of the four situations.
    ///
    /// Unknown codes are [`BlockerCategory::Unrecognized`] rather than guessed
    /// at, so a Cosmos release that adds a code degrades to "shown but not
    /// classified" instead of being silently mis-explained.
    #[must_use]
    pub fn of(code: &str) -> Self {
        match code {
            "type-explanation.missing"
            | "graph.invalid"
            | "contract.unsatisfied"
            | "registration.unknown"
            | "configuration.digest-mismatch"
            | "configuration.schema-unknown"
            | "catalog.stale" => Self::Defect,
            "availability.unknown" | "availability.snapshot-stale" => Self::Unobserved,
            "resource.missing"
            | "resource.access-insufficient"
            | "upstream-output.missing"
            | "provider.unavailable"
            | "configuration.binding-unavailable"
            | "capability.missing"
            | "mechanism.unsupported" => Self::HostUnwired,
            "contract.not-requested" => Self::NotApplicable,
            _ => Self::Unrecognized,
        }
    }

    /// Returns this host's one-line statement of what the category means.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Defect => "this build registered it wrongly; no configuration will fix it",
            Self::Unobserved => {
                "nothing here observed what it needs, so admission could not decide"
            }
            Self::HostUnwired => "sound, but this host has not wired what it needs",
            Self::NotApplicable => "runnable, but it answers a different contract",
            Self::Unrecognized => "reported by a newer Cosmos than this host can classify",
        }
    }
}

/// One stable reason a registration cannot run here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Blocker {
    category: BlockerCategory,
    code: String,
    subjects: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl Blocker {
    /// Reads one Cosmos exclusion reason.
    #[must_use]
    pub fn of(reason: &ExclusionReason) -> Self {
        Self {
            category: BlockerCategory::of(reason.code().as_str()),
            code: reason.code().as_str().to_owned(),
            subjects: reason
                .subjects()
                .iter()
                .map(|subject| subject.as_str().to_owned())
                .collect(),
            detail: reason.detail().map(ToOwned::to_owned),
        }
    }

    /// Returns this host's reading of the exclusion code.
    #[must_use]
    pub const fn category(&self) -> BlockerCategory {
        self.category
    }

    /// Returns the stable Cosmos exclusion code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the stable subjects the exclusion implicates — the missing
    /// resource key, the unsatisfied contract, the unavailable provider.
    #[must_use]
    pub fn subjects(&self) -> &[String] {
        &self.subjects
    }

    /// Returns Cosmos's display-only detail, when the reason carried one.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

/// One registration the sealed catalog holds, as a user needs to read it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TopologyEntry {
    id: String,
    title: String,
    summary: String,
    provider: String,
    contracts: Vec<String>,
    runnable: bool,
    blockers: Vec<Blocker>,
}

impl TopologyEntry {
    /// Returns the stable registration identity.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the registration's own human-readable title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Returns the registration's own statement of what it is for.
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Returns the pack, plugin, or embodiment that registered it.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns every semantic contract the registration claims.
    #[must_use]
    pub fn contracts(&self) -> &[String] {
        &self.contracts
    }

    /// Returns whether mechanical admission would accept this registration
    /// here, under at least one contract it claims.
    #[must_use]
    pub const fn runnable(&self) -> bool {
        self.runnable
    }

    /// Returns every reason it cannot run, empty when it can.
    #[must_use]
    pub fn blockers(&self) -> &[Blocker] {
        &self.blockers
    }
}

/// Every registration this build's sealed catalog holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TopologyInventory {
    catalog_generation: u64,
    catalog_digest: String,
    entries: Vec<TopologyEntry>,
}

impl TopologyInventory {
    /// Returns the catalog generation the listing was derived from.
    #[must_use]
    pub const fn catalog_generation(&self) -> u64 {
        self.catalog_generation
    }

    /// Returns the sealed catalog's stable content digest.
    #[must_use]
    pub fn catalog_digest(&self) -> &str {
        &self.catalog_digest
    }

    /// Returns every registration, in the catalog's registration-ID order.
    #[must_use]
    pub fn entries(&self) -> &[TopologyEntry] {
        &self.entries
    }

    /// Returns how many registrations can run here right now.
    #[must_use]
    pub fn runnable_count(&self) -> usize {
        self.entries.iter().filter(|entry| entry.runnable).count()
    }

    /// Renders the listing as the plain text both front ends print.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut rendered = format!(
            "{} topologies registered, {} runnable here (catalog generation {})\n",
            self.entries.len(),
            self.runnable_count(),
            self.catalog_generation,
        );
        for entry in &self.entries {
            rendered.push('\n');
            let mark = if entry.runnable {
                "runnable"
            } else {
                "not runnable"
            };
            rendered.push_str(&format!("{}  [{mark}]\n", entry.id));
            rendered.push_str(&format!("  {} — {}\n", entry.title, entry.summary));
            rendered.push_str(&format!("  registered by {}\n", entry.provider));
            for blocker in &entry.blockers {
                rendered.push_str(&format!(
                    "  blocked: {} ({}): {}\n",
                    blocker.code,
                    blocker.category.describe(),
                    if blocker.subjects.is_empty() {
                        "—".to_owned()
                    } else {
                        blocker.subjects.join(", ")
                    },
                ));
            }
        }
        rendered
    }
}

/// Why an inventory could not be derived.
#[derive(Debug)]
#[non_exhaustive]
pub enum InventoryError {
    /// The availability probe rejected a derived record.
    Availability(ContractError),
    /// Mechanical preflight rejected the catalog it was given.
    Admission(AdmissionBuildError),
}

impl std::fmt::Display for InventoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Availability(error) => error.fmt(formatter),
            Self::Admission(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for InventoryError {}

impl From<ContractError> for InventoryError {
    fn from(error: ContractError) -> Self {
        Self::Availability(error)
    }
}

impl From<AdmissionBuildError> for InventoryError {
    fn from(error: AdmissionBuildError) -> Self {
        Self::Admission(error)
    }
}

/// Derives the inventory from a sealed catalog and an availability snapshot.
///
/// Preflight runs once per contract the catalog declares, and each
/// registration is read under the contracts it actually claims. A registration
/// is runnable when at least one of its own contracts admits it, which is why
/// a second pack's registrations are never reported blocked merely because the
/// coding contract was not the one asked about.
///
/// # Errors
///
/// Returns an error if mechanical preflight rejects the catalog — in practice
/// only when a registration reached a sealed catalog without complete
/// admission metadata, which the catalog provider already refuses to seal.
pub fn derive(
    catalog: &TopologyCatalogSnapshot,
    availability: &RuntimeAvailabilitySnapshot,
) -> Result<TopologyInventory, InventoryError> {
    let mut by_contract: BTreeMap<TopologyContractKey, Vec<(String, AdmissionResult)>> =
        BTreeMap::new();
    for contract in catalog.contracts() {
        let preflight = preflight_catalog(catalog, contract.key(), availability, false)?;
        by_contract.insert(
            contract.key().clone(),
            preflight
                .evaluations()
                .iter()
                .map(|evaluation| {
                    (
                        evaluation.candidate().registration_id().as_str().to_owned(),
                        evaluation.result().clone(),
                    )
                })
                .collect(),
        );
    }

    let entries = catalog
        .candidates()
        .iter()
        .map(|candidate| entry(candidate, &by_contract))
        .collect();

    Ok(TopologyInventory {
        catalog_generation: catalog.generation(),
        catalog_digest: digest_hex(catalog.content_digest()),
        entries,
    })
}

/// Derives the inventory after observing this host, without a coding task.
///
/// Listing must be answerable before anything is asked of the agent, so the
/// availability snapshot is probed from the workspace root directly rather
/// than from an accepted task. It is the same probe the live selection point
/// runs, so the listed verdict and the selected verdict cannot disagree.
///
/// The availability snapshot identity is derived from the catalog digest and
/// the observation time, so listing invents no randomness and two listings of
/// one unchanged catalog at one instant are byte-identical.
///
/// # Errors
///
/// Returns an error if the probe or mechanical preflight rejects a record.
pub fn observe(
    catalog: &TopologyCatalogSnapshot,
    workspace_root: &Path,
    host: HostCapabilities,
    observed_at_ms: u64,
) -> Result<TopologyInventory, InventoryError> {
    let availability = topology::probe_workspace_availability(
        workspace_root,
        snapshot_id(catalog.content_digest(), observed_at_ms)?,
        observed_at_ms,
        host,
    )?;
    derive(catalog, &availability)
}

fn entry(
    candidate: &TopologyCandidate,
    by_contract: &BTreeMap<TopologyContractKey, Vec<(String, AdmissionResult)>>,
) -> TopologyEntry {
    let id = candidate.registration_id().as_str().to_owned();
    let descriptor = candidate.descriptor();

    let mut runnable = false;
    let mut blockers: Vec<Blocker> = Vec::new();
    for claimed in descriptor.contracts() {
        let Some(result) = by_contract
            .get(claimed)
            .and_then(|results| results.iter().find(|(other, _)| other == &id))
            .map(|(_, result)| result)
        else {
            continue;
        };
        if result.is_admitted() {
            runnable = true;
        }
        for reason in result.reasons() {
            let blocker = Blocker::of(reason);
            if !blockers.contains(&blocker) {
                blockers.push(blocker);
            }
        }
    }
    if runnable {
        blockers.clear();
    }
    blockers.sort_by(|left, right| {
        (left.category, &left.code, &left.subjects).cmp(&(
            right.category,
            &right.code,
            &right.subjects,
        ))
    });

    TopologyEntry {
        id,
        title: descriptor.title().to_owned(),
        summary: descriptor.summary().to_owned(),
        provider: candidate.provenance().provider().to_owned(),
        contracts: descriptor
            .contracts()
            .iter()
            .map(ToString::to_string)
            .collect(),
        runnable,
        blockers,
    }
}

fn digest_hex(digest: ContentDigest) -> String {
    let mut rendered = String::with_capacity(71);
    rendered.push_str("sha256:");
    for byte in digest.as_bytes() {
        rendered.push_str(&format!("{byte:02x}"));
    }
    rendered
}

/// Derives a `UUIDv4`-shaped availability identity from what was observed.
fn snapshot_id(
    catalog: ContentDigest,
    observed_at_ms: u64,
) -> Result<AvailabilitySnapshotId, ContractError> {
    let seed = format!("{}#{observed_at_ms}", digest_hex(catalog));
    let digest = ContentDigest::sha256(seed.as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    AvailabilitySnapshotId::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every exclusion code `polaris_cosmos` can emit today is classified.
    ///
    /// The list is the vocabulary read out of Cosmos's admission boundary. If
    /// Cosmos adds a code, this test still passes — the new code degrades to
    /// [`BlockerCategory::Unrecognized`] — but the listing keeps showing it.
    #[test]
    fn every_cosmos_exclusion_code_has_a_category() {
        let codes = [
            "availability.snapshot-stale",
            "availability.unknown",
            "capability.missing",
            "catalog.stale",
            "configuration.binding-unavailable",
            "configuration.digest-mismatch",
            "configuration.schema-unknown",
            "contract.not-requested",
            "contract.unsatisfied",
            "graph.invalid",
            "mechanism.unsupported",
            "provider.unavailable",
            "registration.unknown",
            "resource.access-insufficient",
            "resource.missing",
            "type-explanation.missing",
            "upstream-output.missing",
        ];
        for code in codes {
            assert_ne!(
                BlockerCategory::of(code),
                BlockerCategory::Unrecognized,
                "{code} must be classified"
            );
        }
    }

    #[test]
    fn an_unknown_code_is_shown_rather_than_guessed_at() {
        assert_eq!(
            BlockerCategory::of("quantum.entangled"),
            BlockerCategory::Unrecognized
        );
    }

    #[test]
    fn every_category_states_its_own_meaning() {
        for category in [
            BlockerCategory::Defect,
            BlockerCategory::Unobserved,
            BlockerCategory::HostUnwired,
            BlockerCategory::NotApplicable,
            BlockerCategory::Unrecognized,
        ] {
            assert!(!category.describe().is_empty());
        }
    }
}
