//! Consumer proof for the topology listing surface.
//!
//! The question this file pins is the user-facing one: *what topologies does
//! this build have, and can each of them run here?* The listing must answer it
//! from the catalog this build actually sealed, so the interesting failures are
//! all drift failures — a hand-maintained list, a name that stops appearing, a
//! blocked registration presented as pickable, a reason a user cannot tell
//! apart from a different reason.
//!
//! What each test pins:
//!
//! | Test | Property |
//! |---|---|
//! | `the_listing_holds_every_sealed_registration_and_nothing_else` | The listed set *is* the sealed set, both directions. |
//! | `an_invented_registration_appears_without_touching_the_listing` | A pack this file invents shows up with no edit to listing code. |
//! | `each_registration_carries_its_own_statement_of_what_it_is_for` | Every entry's title and summary come from its own descriptor. |
//! | `two_unwired_capabilities_are_told_apart_by_code_and_subject` | `resource.missing` on the test runner ≠ `provider.unavailable` on the model. |
//! | `an_unobservable_workspace_reads_differently_from_an_unwired_capability` | A probe failure is a third, distinguishable situation. |
//! | `an_excluded_registration_is_never_presented_as_runnable` | Blocked and runnable are exclusive in both directions. |
//! | `wiring_the_host_is_what_changes_the_verdict` | The verdict tracks the observed host, not a constant. |
//! | `listing_leaves_the_sealed_catalog_untouched` | Generation, digest and membership are identical afterwards. |
//! | `listing_starts_nothing_and_writes_nothing` | The workspace root is byte-identical after two listings. |
//! | `the_rendered_text_answers_both_questions_for_every_registration` | The text a user reads carries purpose and verdict per entry. |
//! | `no_listing_source_file_enumerates_the_registrations` | Nothing in the listing surface can name a registration.

use std::collections::BTreeSet;

use polaris::graph::{Graph, GraphSignature};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin};
use polaris::system::server::Server;
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_code_core::cosmos::inventory::{self, BlockerCategory, TopologyInventory};
use polaris_code_core::cosmos::topology::{self, CodingTopologyPackPlugin, HostCapabilities};
use polaris_code_core::cosmos::{behavior_version, provenance};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, AssessmentSummary, ConfigurationValue, ContentDigest,
    ContextTypeId, ContextTypeKey, ContractError, EpistemicBasis, EvidenceKind, EvidenceObjectId,
    EvidenceRef, EvidenceStoreId, PolicyId, PolicyRef, RegistrationSelectionRef,
    ResolvedConfigurationRef, SourceKind, Status, StopReason, SubjectSchema, SubjectSchemaId,
    SubjectSchemaKey,
};
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, RegistrationId, RegistrationProvenance,
    TopologyCatalog, TopologyCatalogPlugin, TopologyCatalogSnapshot, TopologyContract,
    TopologyContractId, TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey,
    TypeExplanationRegistry,
};

/// One instant, chosen by this file, so two listings are comparable.
const OBSERVED_AT_MS: u64 = 1_700_000_000_000;

// ===========================================================================
// A registration this file invents, which no production source can name.
// ===========================================================================

/// A registration no shipped source knows about: proof that listing is
/// derived, not enumerated.
const INVENTED: &str = "test.invented-topology";

/// The pack that registers it, owned by this test file alone.
const INVENTED_PROVIDER: &str = "polaris-code-test.invented-pack";

/// The subject the invented registration concludes over.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct InventedNote {
    note: String,
}

fn invented_subject_schema() -> SubjectSchema<InventedNote> {
    SubjectSchema::new(
        SubjectSchemaKey::new(
            SubjectSchemaId::new("polaris-code-test.invented-note")
                .expect("subject schema ID is valid"),
            behavior_version(),
        ),
        "One note the invented registration concluded over.",
        r#"{"type":"object","required":["note"]}"#,
        |subject: &InventedNote| {
            if subject.note.is_empty() {
                Err("a note must say something".to_owned())
            } else {
                Ok(())
            }
        },
    )
    .expect("subject schema is valid")
}

fn invented_conclusion() -> Result<AssessmentConclusion, ContractError> {
    let assessment = Assessment::new(
        &invented_subject_schema(),
        InventedNote {
            note: "invented".to_owned(),
        },
        Status::Pass,
        vec![
            provenance(SourceKind::AutomatedVerifier, EpistemicBasis::Verified).with_evidence(
                EvidenceRef::new(
                    EvidenceStoreId::new("polaris-code-test.invented-evidence")
                        .expect("evidence store is valid"),
                    EvidenceKind::VerifierOutput,
                    EvidenceObjectId::new("test.invented-note").expect("evidence object is valid"),
                ),
            ),
        ],
    )?;
    let assessments = [assessment];
    let summary = AssessmentSummary::new(
        &assessments,
        PolicyRef::new(
            PolicyId::new("polaris-code-test.combine-notes").expect("policy ID is valid"),
            behavior_version(),
        ),
        ContentDigest::sha256(b"polaris-code-test.combine-notes"),
        vec![provenance(
            SourceKind::CosmosPolicy,
            EpistemicBasis::ReDerived,
        )],
    )?;
    AssessmentConclusion::new(summary, Vec::new(), StopReason::GoalMet, &assessments)
}

#[system]
async fn invented_terminal() -> Result<AssessmentConclusion, SystemError> {
    invented_conclusion().map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// The invented pack's own contract, so nothing here depends on which pack's
/// `Extends<TopologyCatalog>` build ran first — that order is not the
/// `add_plugins` order, as `tests/multi_pack_catalog_proof.rs` records.
fn invented_contract_key() -> TopologyContractKey {
    TopologyContractKey::new(
        TopologyContractId::new("polaris-code-test.invented-assessment")
            .expect("the invented contract ID is valid"),
        behavior_version(),
    )
}

fn invented_contract() -> TopologyContract {
    TopologyContract::new(
        invented_contract_key(),
        GraphSignature::new().produce::<AssessmentConclusion>(),
        "answer a question no shipped pack asks",
        "one immutable assessment conclusion over the invented note",
    )
    .expect("the invented contract metadata is valid")
}

/// Binds only the one output the invented graph produces, under the key the
/// coding pack already fixed process-wide for that Cosmos type.
fn invented_type_explanations() -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_output::<AssessmentConclusion>(ContextTypeKey::new(
            ContextTypeId::new("polaris-code.assessment-conclusion")
                .expect("the conclusion key is valid"),
            behavior_version(),
        ))
        .expect("the invented pack accepts the process-wide conclusion key");
    registry
}

fn invented_registration() -> CompleteTopologyRegistration {
    let registration_id = RegistrationId::new(INVENTED).expect("registration ID is valid");
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new(INVENTED).expect("topology ID is valid"),
            behavior_version(),
        ),
        "Invented topology",
        "Exists only inside this test file, so the listing cannot have been written to expect it.",
    )
    .expect("descriptor metadata is valid")
    .claim(invented_contract_key());
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        ContentDigest::sha256(b"polaris-code-test.invented-revision"),
        ResolvedConfigurationRef::inline(
            topology::configuration_schema(),
            ConfigurationValue::String(INVENTED.to_owned()),
        )
        .expect("the invented configuration resolves"),
    )
    .expect("selection metadata is valid");
    let mut graph = Graph::new();
    graph.add_system(invented_terminal);
    CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new(INVENTED_PROVIDER).expect("provider ID is valid"),
        CandidateRequirements::none(),
        invented_type_explanations(),
        Vec::new(),
    )
}

/// An ordinary plugin, added alongside the coding pack like any other.
#[derive(Debug, Default, Copy, Clone)]
struct InventedPackPlugin;

#[plugin(id = "polaris_code_test::invented_pack", version = "0.1.0")]
impl Plugin for InventedPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        let build = catalog
            .register_contract(invented_contract())
            .and_then(|()| catalog.register_complete(invented_registration()));
        if let Err(error) = build {
            panic!("the invented pack failed to register: {error}");
        }
    }
}

// ===========================================================================
// Sealing the catalog the way the binary does.
// ===========================================================================

async fn sealed_coding_catalog() -> TopologyCatalogSnapshot {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(CodingTopologyPackPlugin);
    server.finish().await.expect("the coding pack assembles");
    server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog seals at ready")
}

async fn sealed_with_the_invented_pack() -> TopologyCatalogSnapshot {
    let mut server = Server::new();
    server.add_plugins(TopologyCatalogPlugin);
    server.add_plugins(CodingTopologyPackPlugin);
    server.add_plugins(InventedPackPlugin);
    server.finish().await.expect("both packs assemble");
    server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog seals at ready")
}

fn listed(catalog: &TopologyCatalogSnapshot, root: &std::path::Path) -> TopologyInventory {
    listed_on(catalog, root, HostCapabilities::hermetic())
}

fn listed_on(
    catalog: &TopologyCatalogSnapshot,
    root: &std::path::Path,
    host: HostCapabilities,
) -> TopologyInventory {
    inventory::observe(catalog, root, host, OBSERVED_AT_MS).expect("the listing is derivable")
}

fn ids(listing: &TopologyInventory) -> BTreeSet<String> {
    listing
        .entries()
        .iter()
        .map(|entry| entry.id().to_owned())
        .collect()
}

fn sealed_ids(catalog: &TopologyCatalogSnapshot) -> BTreeSet<String> {
    catalog
        .candidates()
        .iter()
        .map(|candidate| candidate.registration_id().as_str().to_owned())
        .collect()
}

fn blockers_of(
    listing: &TopologyInventory,
    id: &str,
) -> Vec<(BlockerCategory, String, Vec<String>)> {
    listing
        .entries()
        .iter()
        .find(|entry| entry.id() == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
        .blockers()
        .iter()
        .map(|blocker| {
            (
                blocker.category(),
                blocker.code().to_owned(),
                blocker.subjects().to_vec(),
            )
        })
        .collect()
}

// ===========================================================================
// 1. The listed set is the sealed set.
// ===========================================================================

#[tokio::test]
async fn the_listing_holds_every_sealed_registration_and_nothing_else() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let listing = listed(&catalog, workspace.path());

    assert_eq!(
        ids(&listing),
        sealed_ids(&catalog),
        "the listed set must be the sealed set, in both directions"
    );
    assert_eq!(
        listing.entries().len(),
        catalog.candidates().len(),
        "no registration may be listed twice or dropped"
    );
    assert_eq!(listing.catalog_generation(), catalog.generation());
}

// ===========================================================================
// 2. A registration this file invents needs no edit to the listing.
// ===========================================================================

#[tokio::test]
async fn an_invented_registration_appears_without_touching_the_listing() {
    let coding_only = sealed_coding_catalog().await;
    let with_invented = sealed_with_the_invented_pack().await;
    let workspace = tempfile::tempdir().expect("temp workspace");

    let before = listed(&coding_only, workspace.path());
    let after = listed(&with_invented, workspace.path());

    assert!(
        !ids(&before).contains(INVENTED),
        "the invented registration must not be in the coding-only catalog"
    );
    assert_eq!(
        ids(&after),
        sealed_ids(&with_invented),
        "the listing tracks whatever the catalog sealed"
    );
    assert!(
        ids(&after).contains(INVENTED),
        "a registration invented in a test file must appear with no listing change"
    );
    assert_eq!(
        after.entries().len(),
        before.entries().len() + 1,
        "exactly one registration was added"
    );

    let invented = after
        .entries()
        .iter()
        .find(|entry| entry.id() == INVENTED)
        .expect("the invented registration is listed");
    assert_eq!(invented.provider(), INVENTED_PROVIDER);
    assert!(
        invented.runnable(),
        "the invented registration needs nothing this host lacks: {:?}",
        invented.blockers()
    );
    assert!(
        after.to_text().contains(INVENTED),
        "the text a user reads names it too"
    );
}

// ===========================================================================
// 3. Purpose comes from the registration, never from the listing.
// ===========================================================================

#[tokio::test]
async fn each_registration_carries_its_own_statement_of_what_it_is_for() {
    let catalog = sealed_with_the_invented_pack().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let listing = listed(&catalog, workspace.path());

    for entry in listing.entries() {
        let candidate = catalog
            .candidate(&RegistrationId::new(entry.id()).expect("a listed ID is a valid ID"))
            .expect("every listed entry is a sealed candidate");
        assert_eq!(
            entry.title(),
            candidate.descriptor().title(),
            "{} must show its own title",
            entry.id()
        );
        assert_eq!(
            entry.summary(),
            candidate.descriptor().summary(),
            "{} must show its own summary",
            entry.id()
        );
        assert_eq!(entry.provider(), candidate.provenance().provider());
        assert!(
            !entry.contracts().is_empty(),
            "{} must show what question it answers",
            entry.id()
        );
    }
}

// ===========================================================================
// 4. Two unwired capabilities are two different reasons.
// ===========================================================================

#[tokio::test]
async fn two_unwired_capabilities_are_told_apart_by_code_and_subject() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let listing = listed(&catalog, workspace.path());

    let test_gated = blockers_of(&listing, "code.test-gated-edit");
    let model_proposed = blockers_of(&listing, "code.model-proposed-edit");

    assert!(
        !test_gated.is_empty(),
        "a hermetic host has no test runner, so the test-gated shape is blocked"
    );
    assert!(
        !model_proposed.is_empty(),
        "a hermetic host has no model provider, so that shape is blocked"
    );
    assert_ne!(
        test_gated, model_proposed,
        "the two reasons must not read identically"
    );

    assert!(
        test_gated.iter().any(|(category, code, subjects)| {
            *category == BlockerCategory::HostUnwired
                && code == "resource.missing"
                && subjects
                    .iter()
                    .any(|subject| subject == "polaris-code.project-test-runner")
        }),
        "the missing test runner must be named as the subject: {test_gated:?}"
    );
    assert!(
        model_proposed.iter().any(|(category, code, subjects)| {
            *category == BlockerCategory::HostUnwired
                && code == "provider.unavailable"
                && subjects
                    .iter()
                    .any(|subject| subject == "polaris-code.anthropic")
        }),
        "the unconfigured provider must be named as the subject: {model_proposed:?}"
    );

    let text = listing.to_text();
    assert!(
        text.contains("polaris-code.project-test-runner")
            && text.contains("polaris-code.anthropic"),
        "a user reading the text can tell the two apart without reading Rust"
    );
    assert!(
        text.contains(BlockerCategory::HostUnwired.describe()),
        "the text says what kind of situation an unwired capability is"
    );
}

// ===========================================================================
// 5. A probe failure is a third, distinguishable situation.
// ===========================================================================

#[tokio::test]
async fn an_unobservable_workspace_reads_differently_from_an_unwired_capability() {
    let catalog = sealed_coding_catalog().await;
    let present = tempfile::tempdir().expect("temp workspace");
    let absent = present.path().join("never-created");

    let observed = listed(&catalog, present.path());
    let unobserved = listed(&catalog, &absent);

    assert!(
        observed.runnable_count() > 0,
        "the shapes needing only a writable workspace run when the probe sees one"
    );
    assert_eq!(
        unobserved.runnable_count(),
        0,
        "no coding shape can run when the workspace itself was not observed"
    );

    let workspace_blocked = blockers_of(&unobserved, "code.direct-edit");
    assert!(
        workspace_blocked.iter().any(|(_, _, subjects)| {
            subjects
                .iter()
                .any(|subject| subject == "polaris-code.coding-workspace")
        }),
        "the failed probe must name the workspace as the subject: {workspace_blocked:?}"
    );
    assert!(
        !workspace_blocked.iter().any(|(_, _, subjects)| {
            subjects
                .iter()
                .any(|subject| subject == "polaris-code.project-test-runner")
        }),
        "a probe failure must not be confused with the missing test runner"
    );
    assert_ne!(
        workspace_blocked,
        blockers_of(&observed, "code.test-gated-edit"),
        "the three situations must not collapse into one reading"
    );
}

// ===========================================================================
// 6. Blocked and runnable are exclusive, in both directions.
// ===========================================================================

#[tokio::test]
async fn an_excluded_registration_is_never_presented_as_runnable() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let listing = listed(&catalog, workspace.path());

    for entry in listing.entries() {
        assert_eq!(
            entry.runnable(),
            entry.blockers().is_empty(),
            "{} claims runnable={} with blockers {:?}",
            entry.id(),
            entry.runnable(),
            entry.blockers()
        );
    }

    let blocked: Vec<&str> = listing
        .entries()
        .iter()
        .filter(|entry| !entry.runnable())
        .map(|entry| entry.id())
        .collect();
    assert!(
        blocked.contains(&"code.test-gated-edit") && blocked.contains(&"code.model-proposed-edit"),
        "a hermetic host cannot run either capability-dependent shape: {blocked:?}"
    );
    assert_eq!(
        listing.runnable_count(),
        listing.entries().len() - blocked.len(),
        "the runnable count is the count of unblocked registrations"
    );

    for id in blocked {
        let marker = format!("{id}  [not runnable]");
        assert!(
            listing.to_text().contains(&marker),
            "{id} must read as not runnable, not merely carry a footnote"
        );
    }
}

// ===========================================================================
// 7. The verdict tracks the host, not a constant.
// ===========================================================================

#[tokio::test]
async fn wiring_the_host_is_what_changes_the_verdict() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");

    let hermetic = listed_on(&catalog, workspace.path(), HostCapabilities::hermetic());
    let wired = listed_on(
        &catalog,
        workspace.path(),
        HostCapabilities {
            test_runner: true,
            model_provider: true,
        },
    );

    assert!(
        blockers_of(&wired, "code.test-gated-edit").is_empty(),
        "wiring a test runner is what makes the test-gated shape runnable"
    );
    assert!(
        blockers_of(&wired, "code.model-proposed-edit").is_empty(),
        "configuring a provider is what makes the model shape runnable"
    );
    assert!(
        wired.runnable_count() > hermetic.runnable_count(),
        "a better-wired host must be able to run more, not the same"
    );
    assert_eq!(
        ids(&wired),
        ids(&hermetic),
        "wiring changes verdicts, never membership"
    );
}

// ===========================================================================
// 8 & 9. Listing is read-only: nothing registered, nothing started.
// ===========================================================================

#[tokio::test]
async fn listing_leaves_the_sealed_catalog_untouched() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");

    let generation = catalog.generation();
    let digest = catalog.content_digest();
    let members = sealed_ids(&catalog);
    let contracts = catalog.contracts().len();

    let first = listed(&catalog, workspace.path());
    let second = listed(&catalog, workspace.path());

    assert_eq!(catalog.generation(), generation, "nothing re-registered");
    assert_eq!(catalog.content_digest(), digest, "the seal is unchanged");
    assert_eq!(sealed_ids(&catalog), members, "membership is unchanged");
    assert_eq!(catalog.contracts().len(), contracts);
    assert_eq!(
        first, second,
        "two listings of one unchanged catalog are identical"
    );
    assert_eq!(first.catalog_generation(), generation);
}

#[tokio::test]
async fn listing_starts_nothing_and_writes_nothing() {
    let catalog = sealed_coding_catalog().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    std::fs::write(workspace.path().join("witness.txt"), b"untouched")
        .expect("the witness file is writable");

    let before: BTreeSet<String> = std::fs::read_dir(workspace.path())
        .expect("the workspace is readable")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let _ = listed(&catalog, workspace.path());
    let _ = listed(&catalog, workspace.path());

    let after: BTreeSet<String> = std::fs::read_dir(workspace.path())
        .expect("the workspace is readable")
        .map(|entry| {
            entry
                .expect("a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    assert_eq!(
        before, after,
        "listing must not execute a graph, so nothing may appear in the workspace"
    );
    assert_eq!(
        std::fs::read(workspace.path().join("witness.txt")).expect("the witness survives"),
        b"untouched",
        "no edit shape ran"
    );
}

// ===========================================================================
// 10. What the user actually reads.
// ===========================================================================

#[tokio::test]
async fn the_rendered_text_answers_both_questions_for_every_registration() {
    let catalog = sealed_with_the_invented_pack().await;
    let workspace = tempfile::tempdir().expect("temp workspace");
    let listing = listed(&catalog, workspace.path());
    let text = listing.to_text();

    for entry in listing.entries() {
        assert!(text.contains(entry.id()), "{} is named", entry.id());
        assert!(
            text.contains(entry.summary()),
            "{} says what it is for",
            entry.id()
        );
        let verdict = if entry.runnable() {
            format!("{}  [runnable]", entry.id())
        } else {
            format!("{}  [not runnable]", entry.id())
        };
        assert!(
            text.contains(&verdict),
            "{} says whether it can run here",
            entry.id()
        );
    }
    assert!(
        text.contains(&format!(
            "{} topologies registered",
            listing.entries().len()
        )),
        "the header counts what this build has"
    );
}

// ===========================================================================
// 11. The listing surface cannot contain the list.
// ===========================================================================

#[test]
fn no_listing_source_file_enumerates_the_registrations() {
    let sources = [
        "src/cosmos/inventory.rs",
        "src/terminal/topologies.rs",
        "src/exec/report.rs",
    ];
    for relative in sources {
        let file = std::fs::read_to_string(relative)
            .unwrap_or_else(|error| panic!("{relative} is readable: {error}"));
        // Only the shipped code is under this rule; a file's own `#[cfg(test)]`
        // module is free to name a registration in a fixture.
        let source = file
            .split_once("#[cfg(test)]")
            .map_or(file.as_str(), |(shipped, _)| shipped);
        for registration in [
            "code.direct-edit",
            "code.test-gated-edit",
            "code.model-proposed-edit",
            "code.prediction-error-edit",
            "code.worklist-sweep-edit",
            "code.coverage-audit",
        ] {
            assert!(
                !source.contains(registration),
                "{relative} names {registration}: the listing would be a hand-maintained table"
            );
        }
    }
}
