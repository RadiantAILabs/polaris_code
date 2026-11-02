//! Proof that the default conversational agent loop is *published* in the
//! topology catalog, and that publishing it activated nothing.
//!
//! The loop [`CodeAgent`] builds is the only way a conversational turn is
//! served, and the catalog could not originally name it. It is published in
//! the catalog "on the same terms as any other registration" without changing
//! which path serves any turn, and the existing coding contract cannot admit
//! it.
//!
//! | Test | Property |
//! |---|---|
//! | `the_coding_contract_cannot_admit_the_default_loop` | The diagnosis, executable: two independent `satisfies` violations, and the catalog really refuses. |
//! | `the_sealed_catalog_lists_the_default_loop` | Whatever enumerates registrations finds it, described, with complete admission metadata. |
//! | `the_default_loops_availability_is_answered_both_ways` | Available on a wired host, `provider.unavailable` on a hermetic one. |
//! | `the_default_loop_is_defined_in_exactly_one_place` | Structural: one `add_loop` site in `src/`, and the registered graph is the one `CodeAgent` builds. |
//! | `publishing_the_default_loop_does_not_change_a_conversational_turn` | Same final text, turn number, node count, usage record, and canonical transcript, with and without the pack. |
//! | `the_published_loop_cannot_win_a_coding_selection` | Under the coding contract it is excluded `contract.not-requested` and never scored. |
//! | `the_coding_registrations_are_unchanged_by_the_new_pack` | Same exclusion codes, same winners, same score components. |
//! | `an_accepted_coding_task_still_runs_the_same_topology` | The live selection path is unaffected. |
//! | `a_turn_is_served_when_the_catalog_is_empty` | The conversational path never needs the catalog. |
//! | `a_turn_is_served_when_every_registration_is_unavailable` | Nothing admissible anywhere, and the turn still completes. |

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use polaris::agent::Agent;
use polaris::graph::Graph;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::cosmos::default_loop::{
    self, CONVERSATION_CONTRACT, ConversationalHost, DEFAULT_LOOP, DefaultLoopTopologyPackPlugin,
};
use polaris_code::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn,
};
use polaris_code::cosmos::policy;
use polaris_code::cosmos::task::{CodingIntent, CodingObjective, CodingTask};
use polaris_code::cosmos::topology::{
    self, CodingTopologyPackPlugin, HostCapabilities, PREDICTION_ERROR,
};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnOutcome, TurnReason};
use polaris_code::usage::TokenMeter;
use polaris_cosmos::contracts::{
    AvailabilitySnapshotId, RegistrationSelectionRef, RuntimeAvailabilitySnapshot, ScoreComponent,
    SelectionDecision, SelectionDecisionId, SelectionOutcome,
};
use polaris_cosmos::evidence::{BoundedInMemoryEvidence, TopologyEvidence, TopologyEvidencePlugin};
use polaris_cosmos::selection::deterministic::{self, SelectionInputs};
use polaris_cosmos::topology::{
    CandidateAdmissionSet, CandidateRequirements, CompleteTopologyRegistration, RegistrationId,
    RegistrationProvenance, TopologyCatalog, TopologyCatalogError, TopologyCatalogSnapshot,
    TopologyContractKey, TopologyDescriptor, TopologyId, TopologyKey, preflight_catalog,
};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The answer the scripted provider streams for a conversational turn.
const ANSWER: &str = "nothing to change here.";

const ADD_STUB: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    todo!()\n}";
const ADD_DONE: &str = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}";

const LIB_RS: &str = "\
//! A tiny crate with one unimplemented helper.

/// Adds two numbers.
pub fn add(a: i32, b: i32) -> i32 {
    todo!()
}
";

// ===========================================================================
// Harness: the same server `main.rs` assembles, with the new pack optional.
// ===========================================================================

/// A provider that streams one fixed answer, so a conversational turn can
/// complete with no API key and no network.
struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(ANSWER)],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ANSWER.to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] the same way `AnthropicPlugin` registers its own.
struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(ScriptedProvider);
    }
}

/// Which topology packs the assembled server installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Packs {
    /// Catalog provider + coding pack only, without the published default loop.
    CodingOnly,
    /// The same, plus the published default loop.
    CodingAndDefaultLoop,
    /// The catalog provider alone — a sealed catalog with nothing in it.
    Empty,
}

/// A temporary crate plus the coding task that implements its `add` stub.
fn project() -> (tempfile::TempDir, CodingTask) {
    let dir = tempfile::tempdir().expect("temp project");
    std::fs::create_dir_all(dir.path().join("src")).expect("src dir");
    std::fs::write(dir.path().join("src/lib.rs"), LIB_RS).expect("seed lib.rs");
    let task = CodingTask::new(
        "implement-add",
        dir.path(),
        "src/lib.rs",
        ADD_STUB,
        ADD_DONE,
        "todo!()",
        CodingIntent::Careful,
    )
    .expect("the coding task is well formed");
    (dir, task)
}

/// Builds and finishes the server `main.rs` builds, with `packs` installed.
async fn selecting_server(
    data_dir: &Path,
    packs: Packs,
) -> (
    Server,
    CodingTaskQueue,
    Arc<ExecIOProvider>,
    mpsc::Sender<IOMessage>,
) {
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(polaris_cosmos::topology::TopologyCatalogPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));
    match packs {
        Packs::Empty => {}
        Packs::CodingOnly => {
            server.add_plugins(CodingTopologyPackPlugin);
        }
        Packs::CodingAndDefaultLoop => {
            server
                .add_plugins(CodingTopologyPackPlugin)
                .add_plugins(DefaultLoopTopologyPackPlugin);
        }
    }

    server
        .finish()
        .await
        .expect("the selecting server assembles");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    (server, queue, provider, input_tx)
}

/// Creates the one session every test drives, exactly as the front ends do.
fn open_session(server: &Server, workspace: &Path) -> SessionId {
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new("scripted/test-model", workspace.to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &agent_type,
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .expect("the session is created");
    session_id
}

/// What one conversational turn observably did, for with/without comparison.
#[derive(Debug, PartialEq)]
struct TurnEvidence {
    reason: TurnReason,
    final_text: Option<String>,
    turn_number: polaris::sessions::TurnNumber,
    nodes_executed: usize,
    selected_a_topology: bool,
    started_a_run: bool,
    usage: serde_json::Value,
    transcript: serde_json::Value,
}

impl TurnEvidence {
    async fn capture(
        sessions: &SessionsAPI,
        session_id: &SessionId,
        outcome: &TurnOutcome,
    ) -> Self {
        let transcript = sessions
            .with_context(session_id, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .map(|manager| {
                        serde_json::to_value(&manager.messages)
                            .expect("canonical messages serialize")
                    })
                    .unwrap_or(serde_json::Value::Null)
            })
            .await
            .unwrap_or(serde_json::Value::Null);
        Self {
            reason: outcome.reason.clone(),
            final_text: outcome.final_text.clone(),
            turn_number: outcome.turn_number,
            nodes_executed: outcome.nodes_executed,
            selected_a_topology: outcome.topology.is_some(),
            // The run id itself is freshly minted per execution, so only its
            // presence can be compared.
            started_a_run: outcome.run_id.is_some(),
            usage: serde_json::to_value(outcome.usage.as_ref())
                .expect("the usage record serializes"),
            transcript,
        }
    }
}

/// Drives one ordinary conversational turn against a server with `packs`.
async fn conversational_turn(packs: Packs) -> TurnEvidence {
    let (project_dir, _task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, provider, input_tx) = selecting_server(data_dir.path(), packs).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    assert!(queue.is_empty(), "nothing is accepted: this is a chat turn");
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("is anything wrong?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    drop(input_tx);

    let outcome = run::drive_turn(sessions, &session_id).await;
    provider.close().await;
    TurnEvidence::capture(sessions, &session_id, &outcome).await
}

fn sealed(server: &Server) -> TopologyCatalogSnapshot {
    server
        .get_global::<TopologyCatalog>()
        .expect("the provider globalizes the catalog")
        .snapshot()
        .expect("the catalog is sealed")
}

/// A sealed catalog holding both packs, assembled without a whole server.
fn sealed_both_packs() -> TopologyCatalogSnapshot {
    let mut catalog = TopologyCatalog::new();
    topology::contract()
        .and_then(|contract| {
            catalog
                .register_contract(contract)
                .map_err(std::convert::Into::into)
        })
        .expect("the coding contract registers");
    // The coding pack has no free registration entry point, so the packs are
    // assembled through a server wherever both are needed together; this
    // helper is only used where the default loop alone is under test.
    default_loop::register_default_loop(&mut catalog).expect("the default loop registers");
    catalog
        .seal_admission_ready()
        .expect("every registration is admission-ready")
}

fn coding_contract_key() -> TopologyContractKey {
    topology::contract()
        .expect("the coding contract is well formed")
        .key()
        .clone()
}

fn availability_id(suffix: &str) -> AvailabilitySnapshotId {
    format!("d0d0d0d0-0000-4000-8000-0000000000{suffix}")
        .parse()
        .expect("availability ID is a UUIDv4")
}

fn decision_id(suffix: &str) -> SelectionDecisionId {
    format!("d0d0d0d0-0000-4000-8000-00000000010{suffix}")
        .parse()
        .expect("decision ID is a UUIDv4")
}

/// Exclusion codes per registration, in canonical registration order.
fn exclusion_codes(admission: &CandidateAdmissionSet) -> BTreeMap<String, Vec<String>> {
    admission
        .evaluations()
        .iter()
        .map(|evaluation| {
            (
                evaluation.candidate().registration_id().as_str().to_owned(),
                evaluation
                    .result()
                    .reasons()
                    .iter()
                    .map(|reason| reason.code().as_str().to_owned())
                    .collect(),
            )
        })
        .collect()
}

/// The score components each candidate was ranked with, by registration.
fn scored(decision: &SelectionDecision) -> BTreeMap<String, Vec<ScoreComponent>> {
    decision
        .candidates()
        .iter()
        .map(|consideration| {
            (
                consideration
                    .candidate()
                    .registration_id()
                    .as_str()
                    .to_owned(),
                consideration
                    .score()
                    .map(|score| score.components().to_vec())
                    .unwrap_or_default(),
            )
        })
        .collect()
}

fn winner(decision: &SelectionDecision) -> String {
    match decision.outcome() {
        SelectionOutcome::Selected { selected, .. } => {
            selected.registration_id().as_str().to_owned()
        }
        SelectionOutcome::NoEligibleCandidate { diagnostics } => {
            panic!("expected a winner, got diagnostics {diagnostics:?}")
        }
    }
}

/// The host's own coding selection path, over whichever snapshot it is handed.
fn select_coding(
    snapshot: &TopologyCatalogSnapshot,
    availability: &RuntimeAvailabilitySnapshot,
    task: &CodingTask,
    intent: CodingIntent,
    decision: SelectionDecisionId,
) -> (CandidateAdmissionSet, SelectionDecision) {
    let contract = coding_contract_key();
    let admission = preflight_catalog(snapshot, &contract, availability, false)
        .expect("preflight reads every candidate from the sealed catalog");
    let statistics = TopologyEvidence::new(Arc::new(BoundedInMemoryEvidence::new(
        policy::evidence_config(),
    )))
    .capture_statistics(&policy::statistics_request())
    .expect("an empty evidence store captures empty statistics");
    let inputs = SelectionInputs::new(
        decision,
        task.profile().expect("the profile aggregates"),
        CodingObjective::new(intent)
            .build()
            .expect("the objective builds"),
        contract,
        snapshot.clone(),
        availability.clone(),
        admission.clone(),
        statistics,
        policy::candidate_facts(snapshot, None).expect("selector facts build"),
        policy::selector_config().expect("the selector configuration builds"),
    );
    let decision = deterministic::select(&inputs).expect("selection completes");
    (admission, decision)
}

/// The six registrations the coding pack has always contributed.
const CODING_REGISTRATIONS: [&str; 6] = [
    topology::COVERAGE_AUDIT,
    topology::DIRECT,
    topology::MODEL_PROPOSED,
    topology::PREDICTION_ERROR,
    topology::TEST_GATED,
    topology::WORKLIST_SWEEP,
];

// ===========================================================================
// 1. The diagnosis: the existing contract cannot admit it.
// ===========================================================================

#[test]
fn the_coding_contract_cannot_admit_the_default_loop() {
    // Confirmed against the real graph rather than read off the source: the
    // loop's derived signature violates the coding contract on two independent
    // axes, and neither is repairable without restructuring the loop or
    // widening a contract six other registrations already satisfy.
    let (missing_produces, extra_requires_outputs) =
        default_loop::assert_default_loop_does_not_satisfy_the_coding_contract();
    assert_eq!(
        missing_produces.len(),
        1,
        "expected exactly the conclusion to be missing, got {missing_produces:?}"
    );
    assert!(
        missing_produces[0].contains("AssessmentConclusion"),
        "a conversational turn does not terminate in a coding judgement: {missing_produces:?}"
    );
    assert_eq!(
        extra_requires_outputs.len(),
        1,
        "expected exactly the caught error, got {extra_requires_outputs:?}"
    );
    assert!(
        extra_requires_outputs[0].contains("CaughtError"),
        "the loop's error handler reads a free output the coding contract never sanctions: \
         {extra_requires_outputs:?}"
    );

    // And the catalog itself refuses, with the typed error that names the diff.
    let mut catalog = TopologyCatalog::new();
    catalog
        .register_contract(topology::contract().expect("the coding contract is well formed"))
        .expect("the coding contract registers");
    let registration_id =
        RegistrationId::new("code.default-agent-loop-under-coding").expect("a valid stable name");
    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new("code.default-agent-loop-under-coding").expect("a valid stable name"),
            Version::new(1, 0, 0),
        ),
        "Default agent loop under the coding contract",
        "The published loop, claiming a contract it cannot satisfy.",
    )
    .expect("descriptor metadata is valid")
    .claim(coding_contract_key());
    let selection = RegistrationSelectionRef::new(
        registration_id,
        descriptor.key().clone(),
        polaris_cosmos::contracts::ContentDigest::sha256(b"polaris-code.revision:probe"),
        polaris_cosmos::contracts::ResolvedConfigurationRef::inline(
            default_loop::configuration_schema(),
            polaris_cosmos::contracts::ConfigurationValue::String("probe".to_owned()),
        )
        .expect("configuration is valid"),
    )
    .expect("selection metadata is valid");
    let error = catalog
        .register_complete(CompleteTopologyRegistration::new(
            selection,
            descriptor,
            default_loop::default_loop_graph(),
            RegistrationProvenance::new(default_loop::PACK_PROVIDER).expect("a valid provider"),
            CandidateRequirements::none(),
            default_loop::type_explanations().expect("the explanations build"),
            default_loop::author_priors(),
        ))
        .expect_err("the coding contract must refuse the default loop");
    let TopologyCatalogError::UnsatisfiedContract { diff, .. } = &error else {
        panic!("expected UnsatisfiedContract, got {error:?}");
    };
    assert_eq!(diff.missing_produces().len(), 1);
    assert_eq!(diff.extra_requires_outputs().len(), 1);
}

// ===========================================================================
// 2. It is in the catalog, on the same terms as any other registration.
// ===========================================================================

#[tokio::test]
async fn the_sealed_catalog_lists_the_default_loop() {
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, _queue, _provider, _input_tx) =
        selecting_server(data_dir.path(), Packs::CodingAndDefaultLoop).await;
    let snapshot = sealed(&server);

    // Seven coding registrations and the default loop, one catalog, two
    // contracts.
    assert_eq!(snapshot.candidates().len(), 8);
    assert_eq!(snapshot.contracts().len(), 2);
    assert!(
        snapshot
            .contracts()
            .iter()
            .any(|contract| contract.key().id().as_str() == CONVERSATION_CONTRACT)
    );

    let candidate = snapshot
        .candidates()
        .iter()
        .find(|candidate| candidate.registration_id().as_str() == DEFAULT_LOOP)
        .expect("whatever enumerates registrations must find the default loop");

    // Discoverable and describable on the same terms as the others.
    assert_eq!(candidate.descriptor().title(), "Default agent loop");
    assert!(
        candidate.descriptor().summary().contains("conversational"),
        "the summary must say what it is: {}",
        candidate.descriptor().summary()
    );
    assert_eq!(
        candidate.provenance().provider(),
        default_loop::PACK_PROVIDER
    );
    let metadata = candidate
        .admission_metadata()
        .expect("the default loop registered completely, like every other candidate");
    assert_eq!(
        metadata.selection().registration_id().as_str(),
        DEFAULT_LOOP
    );

    // The signature explanation is the loop's real boundary, not a summary of
    // it: thirty-five resource accesses over twenty-eight types (seven are
    // read and written), one sanctioned free output read, seven produced
    // outputs.
    let explanation = metadata.signature_explanation();
    assert_eq!(explanation.required_resources().len(), 35);
    assert_eq!(
        explanation
            .required_resources()
            .iter()
            .map(|requirement| requirement.type_key().clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        28
    );
    assert_eq!(explanation.required_outputs().len(), 1);
    assert_eq!(explanation.produced_outputs().len(), 7);

    // It claims nothing about its own worth. The three prior-free coding
    // registrations already establish that this is a legal thing to claim.
    assert!(
        metadata.author_priors().is_empty(),
        "the default loop must not carry invented priors: {:?}",
        metadata.author_priors()
    );

    // Host assembly is one more `add_plugins` line bound to the catalog
    // capability, not an edge into the coding pack.
    let entry = server
        .plugin_manifest()
        .entry(&PluginId::new(DefaultLoopTopologyPackPlugin::ID))
        .expect("the host enables the default loop pack");
    assert_eq!(entry.extends().len(), 1);
    assert_eq!(
        entry.extends()[0].provider(),
        Some(&PluginId::new(
            polaris_cosmos::topology::TopologyCatalogPlugin::ID
        ))
    );
}

#[test]
fn the_default_loops_availability_is_answered_both_ways() {
    let snapshot = sealed_both_packs();
    let contract = default_loop::contract_key();

    // A host with the session installed and a provider configured: admissible.
    let live =
        default_loop::probe_availability(availability_id("01"), 1_000, ConversationalHost::live())
            .expect("the host observes its own capabilities");
    let admitted = preflight_catalog(&snapshot, &contract, &live, false).expect("preflight runs");
    assert_eq!(
        exclusion_codes(&admitted)[DEFAULT_LOOP],
        Vec::<String>::new(),
        "a wired host must admit the default loop"
    );

    // A hermetic host: the answer is "no", and it says exactly why. This is
    // the negative case that makes "available" mean something.
    let hermetic = default_loop::probe_availability(
        availability_id("02"),
        1_000,
        ConversationalHost::hermetic(),
    )
    .expect("the host observes its own capabilities");
    let refused =
        preflight_catalog(&snapshot, &contract, &hermetic, false).expect("preflight runs");
    let codes = &exclusion_codes(&refused)[DEFAULT_LOOP];
    assert!(
        codes.contains(&"provider.unavailable".to_owned()),
        "a host with no model provider cannot serve a turn: {codes:?}"
    );
    assert_eq!(
        codes
            .iter()
            .filter(|code| *code == "resource.missing")
            .count(),
        28,
        "every uninstalled session resource is reported, not just the first: {codes:?}"
    );
    assert!(
        codes.contains(&"upstream-output.missing".to_owned()),
        "the sanctioned free output read is answered too: {codes:?}"
    );
}

// ===========================================================================
// 3. One definition of the loop, not two.
// ===========================================================================

#[test]
fn the_default_loop_is_defined_in_exactly_one_place() {
    // Structural: the loop is wired in exactly one place under `src/`, and it
    // is not this pack. A second `add_loop` site would be the failure mode
    // this feature is most at risk of producing.
    let src = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut sites = Vec::new();
    let mut pack_source = String::new();
    scan_rust_sources(&src, &mut |path, contents| {
        if contents.contains("add_loop::<LoopState") {
            sites.push(path.to_path_buf());
        }
        if path.ends_with("cosmos/default_loop.rs") {
            pack_source = contents.to_owned();
        }
    });
    assert_eq!(
        sites.len(),
        1,
        "the default loop must be wired in exactly one place, found {sites:?}"
    );
    assert!(sites[0].ends_with("agents/code_agent.rs"));
    assert!(
        !pack_source.is_empty(),
        "the publication module must be under src/"
    );
    for forbidden in ["add_loop", "add_conditional_branch", "add_error_handler"] {
        assert!(
            !pack_source.contains(forbidden),
            "the publication module reached for `{forbidden}`: it must register the loop, not rebuild it"
        );
    }

    // Behavioural: the registered graph is the one `CodeAgent` builds, node for
    // node and boundary for boundary.
    let mut direct = Graph::new();
    CodeAgent.build(&mut direct);
    let published = default_loop::default_loop_graph();
    assert_eq!(
        format!("{:?}", published.signature()),
        format!("{:?}", direct.signature()),
        "the published graph's boundary drifted from the agent's"
    );
    assert!(published.validate().is_ok());

    // And the graph the *catalog* holds is that same graph.
    let snapshot = sealed_both_packs();
    let candidate = snapshot
        .candidates()
        .iter()
        .find(|candidate| candidate.registration_id().as_str() == DEFAULT_LOOP)
        .expect("the default loop is registered");
    assert_eq!(
        format!("{:?}", candidate.signature()),
        format!("{:?}", direct.signature())
    );
}

fn scan_rust_sources(dir: &Path, visit: &mut impl FnMut(&Path, &str)) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        if path.is_dir() {
            scan_rust_sources(&path, visit);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let contents = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            visit(&path, &contents);
        }
    }
}

// ===========================================================================
// 4. Publication is not activation.
// ===========================================================================

#[tokio::test]
async fn publishing_the_default_loop_does_not_change_a_conversational_turn() {
    // The same turn, driven twice: once against the assembly without the
    // default loop published, once against the one with it. Everything
    // observable about the turn must be identical — which is the whole claim
    // that publishing it changes which path serves a turn for no input.
    let before = conversational_turn(Packs::CodingOnly).await;
    let after = conversational_turn(Packs::CodingAndDefaultLoop).await;

    assert_eq!(before.reason, TurnReason::Completed);
    assert_eq!(before.final_text.as_deref(), Some(ANSWER));
    assert_eq!(before.turn_number, 1);
    assert!(before.started_a_run);
    assert!(!before.selected_a_topology);
    assert_eq!(
        before, after,
        "publishing the default loop changed an ordinary turn"
    );
}

#[tokio::test]
async fn the_published_loop_cannot_win_a_coding_selection() {
    // Publication does not put it in reach of the one selection point the
    // shipped binary has. Under the coding contract it is filtered out by the
    // generic contract check and never scored — and it carries no priors, so
    // there is nothing to score it with even if a contract did request it.
    let (project_dir, task) = project();
    let _ = project_dir;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, _queue, _provider, _input_tx) =
        selecting_server(data_dir.path(), Packs::CodingAndDefaultLoop).await;
    let snapshot = sealed(&server);
    let availability = topology::probe_availability(
        &task,
        availability_id("03"),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");

    let (admission, decision) = select_coding(
        &snapshot,
        &availability,
        &task,
        CodingIntent::Careful,
        decision_id("1"),
    );
    let codes = exclusion_codes(&admission);
    assert!(
        codes[DEFAULT_LOOP].contains(&"contract.not-requested".to_owned()),
        "the default loop must be filtered out by contract: {:?}",
        codes[DEFAULT_LOOP]
    );
    assert!(
        scored(&decision)[DEFAULT_LOOP].is_empty(),
        "a mechanically excluded candidate must never be scored"
    );
    assert_ne!(winner(&decision), DEFAULT_LOOP);
}

// ===========================================================================
// 5. The seven existing registrations are untouched.
// ===========================================================================

#[tokio::test]
async fn the_coding_registrations_are_unchanged_by_the_new_pack() {
    let (project_dir, task) = project();
    let _ = project_dir;
    let data_dir_before = tempfile::tempdir().expect("temp data dir");
    let data_dir_after = tempfile::tempdir().expect("temp data dir");
    let (before_server, _q1, _p1, _i1) =
        selecting_server(data_dir_before.path(), Packs::CodingOnly).await;
    let (after_server, _q2, _p2, _i2) =
        selecting_server(data_dir_after.path(), Packs::CodingAndDefaultLoop).await;
    let before = sealed(&before_server);
    let after = sealed(&after_server);

    assert_eq!(before.candidates().len(), 7);
    assert_eq!(after.candidates().len(), 8);

    let availability = topology::probe_availability(
        &task,
        availability_id("04"),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");

    for (label, intent, suffix) in [
        ("fast", CodingIntent::Fast, "2"),
        ("careful", CodingIntent::Careful, "3"),
        ("thorough", CodingIntent::Thorough, "4"),
    ] {
        let (before_admission, before_decision) =
            select_coding(&before, &availability, &task, intent, decision_id(suffix));
        let (after_admission, after_decision) =
            select_coding(&after, &availability, &task, intent, decision_id(suffix));

        let before_codes = exclusion_codes(&before_admission);
        let after_codes = exclusion_codes(&after_admission);
        let before_scores = scored(&before_decision);
        let after_scores = scored(&after_decision);
        for id in CODING_REGISTRATIONS {
            assert_eq!(
                before_codes[id], after_codes[id],
                "{id} admitted differently under {label} once the loop was published"
            );
            assert_eq!(
                before_scores[id], after_scores[id],
                "{id} was scored differently under {label} once the loop was published"
            );
        }
        assert_eq!(
            winner(&before_decision),
            winner(&after_decision),
            "the {label} objective picked a different coding topology"
        );
    }
}

#[tokio::test]
async fn an_accepted_coding_task_still_runs_the_same_topology() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, queue, _provider, _input_tx) =
        selecting_server(data_dir.path(), Packs::CodingAndDefaultLoop).await;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&server, project_dir.path());

    queue.accept(AcceptedCodingTask::new(task));
    let outcome = run::drive_turn(sessions, &session_id).await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!("expected an executed topology, got {:?}", outcome.topology);
    };
    assert_eq!(
        executed.registration_id, PREDICTION_ERROR,
        "the careful objective still ranks the prediction-error loop first"
    );
    assert_eq!(executed.fallback_position, 0);
    let after = std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
        .expect("the edited file is readable");
    assert!(after.contains("a + b"), "the selected graph still ran");
}

// ===========================================================================
// 6. The default loop never becomes unreachable.
// ===========================================================================

#[tokio::test]
async fn a_turn_is_served_when_the_catalog_is_empty() {
    // The catalog provider is installed and seals an empty catalog: no packs,
    // no contracts, no registrations. A turn must still be servable, because
    // the conversational path never consults the catalog at all.
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, _queue, _provider, _input_tx) =
        selecting_server(data_dir.path(), Packs::Empty).await;
    let snapshot = sealed(&server);
    assert!(snapshot.candidates().is_empty());
    assert!(snapshot.contracts().is_empty());

    let evidence = conversational_turn(Packs::Empty).await;
    assert_eq!(evidence.reason, TurnReason::Completed);
    assert_eq!(evidence.final_text.as_deref(), Some(ANSWER));
    assert_eq!(evidence.turn_number, 1);
    assert!(!evidence.selected_a_topology);
}

#[tokio::test]
async fn a_turn_is_served_when_every_registration_is_unavailable() {
    let (project_dir, task) = project();
    let _ = project_dir;
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let (server, _queue, _provider, _input_tx) =
        selecting_server(data_dir.path(), Packs::CodingAndDefaultLoop).await;
    let snapshot = sealed(&server);

    // A host that has wired nothing: under each contract in the catalog, every
    // registration that claims it is excluded.
    let coding = topology::probe_availability(
        &task,
        availability_id("05"),
        1_000,
        HostCapabilities::hermetic(),
    )
    .expect("the host observes its own capabilities");
    let conversational = default_loop::probe_availability(
        availability_id("06"),
        1_000,
        ConversationalHost::hermetic(),
    )
    .expect("the host observes its own capabilities");
    // Cross the availability snapshots so nothing anywhere is admissible.
    for (contract, availability) in [
        (coding_contract_key(), &conversational),
        (default_loop::contract_key(), &coding),
    ] {
        let admission =
            preflight_catalog(&snapshot, &contract, availability, false).expect("preflight runs");
        assert!(
            admission
                .evaluations()
                .iter()
                .all(|evaluation| !evaluation.result().reasons().is_empty()),
            "this case is vacuous unless every registration is excluded"
        );
    }

    // And the turn is still served, by the ordinary conversational path.
    let evidence = conversational_turn(Packs::CodingAndDefaultLoop).await;
    assert_eq!(evidence.reason, TurnReason::Completed);
    assert_eq!(evidence.final_text.as_deref(), Some(ANSWER));
    assert!(!evidence.selected_a_topology);
}
