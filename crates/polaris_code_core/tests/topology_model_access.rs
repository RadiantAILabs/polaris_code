//! Proof that a selected topology can call a model, bills its own meter, and
//! still counts against the session's accounting.
//!
//! The philosophy this repository is built on says the graph is the agent, and
//! names a model call as the first kind of node. A topology that cannot reach
//! a model is therefore a missing wire, not a boundary. The boundary that does
//! exist is narrower: a topology run does not *advance* the session — its
//! turn number and transcript are untouched — but its spend is still the
//! session's own spend.
//!
//! This file pins all of that at once, through the live host loop:
//!
//! - a topology whose graph reads `Res<ModelRegistry>` is admitted and really
//!   calls the model;
//! - the tokens it spends land on the run's own [`TokenMeter`], reported as
//!   `ExecutedTopology::usage`, and are also folded onto the session's own
//!   meter and its operator-set spend ceiling; and
//! - after that turn the session's turn number is still zero and its
//!   transcript is still empty.
//!
//! The last two are only worth something if the same session can move them, so
//! a further test runs an ordinary conversational turn against the same
//! server and shows the session meter advancing further then.
//!
//! The pack below is a test-owned pack: it registers the coding contract and
//! one model-calling registration under it. It does not use
//! [`CodingTopologyPackPlugin`], because the shipped `code.model-proposed-edit`
//! registration is a stub whose hermetic exclusion is pinned by
//! `tests/cosmos_contracts_proof.rs`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use polaris::graph::{Graph, GraphSignature};
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason as LlmStopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::param::{Out, Res};
use polaris::system::plugin;
use polaris::system::plugin::{Extends, Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::system::system;
use polaris::system::system::{SystemError, erase};
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code_core::cosmos::assess;
use polaris_code_core::cosmos::composition::CompositionPolicy;
use polaris_code_core::cosmos::composition::{CompositionTemplate, CompositionTemplates};
use polaris_code_core::cosmos::host::{
    AcceptedCodingTask, CodingTaskQueue, CodingTaskQueuePlugin, TopologyTurn,
};
use polaris_code_core::cosmos::payload::{AppliedEdit, CodingWorkspace, EditSite};
use polaris_code_core::cosmos::policy;
use polaris_code_core::cosmos::task::{CodingIntent, CodingTask};
use polaris_code_core::cosmos::topology::{self, HostCapabilities};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::usage::{RunCeiling, TokenMeter};
use polaris_cosmos::cognition::approved::blocks::{self, VerifiedRoles};
use polaris_cosmos::contracts::{
    Assessment, AssessmentConclusion, ContentDigest, LogicalSequence, RecordedAt,
    RegistrationSelectionRef, ResolvedConfigurationRef, StopReason,
};
use polaris_cosmos::evidence::TopologyEvidencePlugin;
use polaris_cosmos::topology::{
    CandidateRequirements, CompleteTopologyRegistration, RegistrationId, RegistrationProvenance,
    TopologyCatalog, TopologyCatalogPlugin, TopologyContractKey, TopologyDescriptor, TopologyId,
    TopologyKey, TypeExplanationRegistry,
};
use polaris_ontrace::oxigraph::OxigraphPlugin;

/// The one model both the topology and the conversational agent drive.
const MODEL: &str = "scripted/test-model";

/// Registration id of the test pack's model-calling topology.
const MODEL_CALLING: &str = "test.model-calling-edit";

/// Provider identity the test pack registers under.
const TEST_PACK: &str = "polaris-code-test.model-calling-pack";

/// Input tokens the counting provider reports for every call.
const INPUT_TOKENS: u64 = 111;

/// Output tokens the counting provider reports for every call.
const OUTPUT_TOKENS: u64 = 7;

/// The answer the counting provider produces.
const ANSWER: &str = "replace the stub body with the sum.";
const MODEL_CHOICE: &str = r#"{"recipe":"polaris-code.recipe.test-model-call","grounds":"the isolated test pack permits this one verified composition"}"#;
const UNKNOWN_MODEL_CHOICE: &str =
    r#"{"recipe":"polaris-code.recipe.not-approved","grounds":"try an unknown graph"}"#;
const OVER_BOUND_MODEL_CHOICE: &str = r#"{"recipe":"polaris-code.recipe.test-model-call","worklist_bound":999,"grounds":"try an invalid bound"}"#;

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
// A provider that counts its calls and reports a known, non-zero usage.
// ===========================================================================

fn scripted_usage() -> Usage {
    Usage {
        input_tokens: Some(INPUT_TOKENS),
        output_tokens: Some(OUTPUT_TOKENS),
        total_tokens: Some(INPUT_TOKENS + OUTPUT_TOKENS),
        ..Default::default()
    }
}

/// A provider with no network that reports a known usage, so "the model was
/// reached" and "the spend was billed" are both observable.
///
/// The call counter is per instance, not a process static: the tests in this
/// file run concurrently in one process and each builds its own server.
struct CountingProvider {
    calls: Arc<AtomicUsize>,
    answer: &'static str,
}

impl LlmProvider for CountingProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(self.answer)],
            usage: scripted_usage(),
            stop_reason: LlmStopReason::EndTurn,
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(self.answer.to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: LlmStopReason::EndTurn,
                usage: scripted_usage(),
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`CountingProvider`] the way `AnthropicPlugin` registers its own.
struct CountingModelPlugin {
    calls: Arc<AtomicUsize>,
    answer: &'static str,
}

impl Plugin for CountingModelPlugin {
    const ID: &'static str = "test::counting_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before CountingModelPlugin")
            .register_llm_provider(CountingProvider {
                calls: self.calls.clone(),
                answer: self.answer,
            });
    }
}

// ===========================================================================
// The test pack: one coding-contract topology whose graph calls the model.
// ===========================================================================

/// The proposing node: ask the model, bill the run's own meter, and apply the edit.
///
/// The three resources are exactly the seam under test. `ModelRegistry` is a
/// global the host attaches to the run context; `TokenMeter` is the per-run
/// meter the host inserts, which shadows the session's global one; and
/// `CodingWorkspace` is the sandbox the host already prepared.
#[system]
async fn model_calling_edit(
    registry: Res<ModelRegistry>,
    meter: Res<TokenMeter>,
    workspace: Res<CodingWorkspace>,
) -> Result<AppliedEdit, SystemError> {
    let llm = registry
        .llm(MODEL)
        .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    let response = llm
        .generate(LlmRequest {
            messages: vec![Message::user("how should this stub be filled in?")],
            ..Default::default()
        })
        .await
        .map_err(|error| SystemError::ExecutionError(error.to_string()))?;

    // The graph accounts its own spend. `Res<TokenMeter>` resolves to the run's
    // meter, never the session's.
    meter.add(
        response.usage.input_tokens.unwrap_or_default(),
        response.usage.output_tokens.unwrap_or_default(),
    );

    workspace
        .apply()
        .map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// Checks the applied edit as the oracle half of the finite verified-step block.
#[system]
async fn model_calling_judge(
    workspace: Res<CodingWorkspace>,
    _applied: Out<AppliedEdit>,
) -> Result<Assessment<EditSite>, SystemError> {
    let actual = workspace
        .verify()
        .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    let status = workspace.judge_verified(&actual);
    assess::site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        status,
    ))
    .map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// Resolves the verified-step observation into the coding contract conclusion.
#[system]
async fn model_calling_conclude(
    workspace: Res<CodingWorkspace>,
    assessment: Out<Assessment<EditSite>>,
) -> Result<AssessmentConclusion, SystemError> {
    let status = assessment.status();
    let assessments = workspace
        .edit_sites(status)
        .into_iter()
        .map(assess::site_assessment)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    assess::conclude_sites(assessments, Vec::new(), StopReason::GoalMet)
        .map_err(|error| SystemError::ExecutionError(error.to_string()))
}

/// Builds the test pack's native composition from the same Cosmos blocks its
/// recipe records, with only the model-calling coding systems supplied here.
fn model_calling_graph() -> Graph {
    let mut graph = Graph::new();
    blocks::verified_step(
        &mut graph,
        erase(model_calling_edit),
        erase(model_calling_judge),
        &polaris_code_core::cosmos::graphs::oracle_qualification(),
        VerifiedRoles::Correction,
    );
    blocks::resolve_residuals(&mut graph, erase(model_calling_conclude), false);
    graph
}

fn coding_contract_key() -> TopologyContractKey {
    topology::contract()
        .expect("the coding contract builds")
        .key()
        .clone()
}

/// Type explanations for the one registration.
///
/// The workspace and conclusion keys are the coding pack's own literals: the
/// host's [`topology::probe_availability`] reports the workspace under exactly
/// that key, so a different spelling here would be `resource.missing`. The
/// model registry and the run meter use the keys the coding vocabulary now
/// publishes for them.
fn model_calling_explanations() -> TypeExplanationRegistry {
    let mut registry = TypeExplanationRegistry::new();
    registry
        .bind_resource::<CodingWorkspace>(topology::type_key("polaris-code.coding-workspace"))
        .expect("the workspace binds once");
    registry
        .bind_resource::<ModelRegistry>(topology::type_key(topology::MODEL_REGISTRY_TYPE))
        .expect("the model registry binds once");
    registry
        .bind_resource::<TokenMeter>(topology::type_key(topology::RUN_METER_TYPE))
        .expect("the run meter binds once");
    registry
        .bind_output::<AppliedEdit>(topology::type_key("polaris-code.applied-edit"))
        .expect("the applied edit binds once");
    registry
        .bind_output::<Assessment<EditSite>>(topology::type_key(
            "polaris-code.assessment-edit-site",
        ))
        .expect("the site assessment binds once");
    registry
        .bind_output::<AssessmentConclusion>(topology::type_key(
            "polaris-code.assessment-conclusion",
        ))
        .expect("the conclusion binds once");
    registry
}

fn model_calling_registration() -> CompleteTopologyRegistration {
    let graph = model_calling_graph();

    let descriptor = TopologyDescriptor::new(
        TopologyKey::new(
            TopologyId::new(MODEL_CALLING).expect("topology ID is valid"),
            Version::new(1, 0, 0),
        ),
        "Model-calling edit",
        "Asks the configured model how to fill the stub, then applies the edit.",
    )
    .expect("descriptor metadata is valid")
    .claim(coding_contract_key());
    let selection = RegistrationSelectionRef::new(
        RegistrationId::new(MODEL_CALLING).expect("registration ID is valid"),
        descriptor.key().clone(),
        ContentDigest::sha256(b"polaris-code-test.revision:model-calling"),
        ResolvedConfigurationRef::inline(
            topology::configuration_schema(),
            polaris_cosmos::contracts::ConfigurationValue::String(MODEL_CALLING.to_owned()),
        )
        .expect("configuration is valid"),
    )
    .expect("selection metadata is valid");

    CompleteTopologyRegistration::new(
        selection,
        descriptor,
        graph,
        RegistrationProvenance::new(TEST_PACK).expect("test provider ID is valid"),
        CandidateRequirements::none(),
        model_calling_explanations(),
        Vec::new(),
    )
}

/// The test-owned pack. It owns the coding contract in this catalog, so it does
/// not depend on any other pack having built first.
#[derive(Debug, Default, Copy, Clone)]
struct ModelCallingPackPlugin;

#[plugin(id = "polaris_code_test::model_calling_pack", version = "0.1.0")]
impl Plugin for ModelCallingPackPlugin {
    fn build(&self, mut catalog: Extends<TopologyCatalog>) {
        catalog
            .register_contract(topology::contract().expect("the coding contract builds"))
            .expect("the test pack owns the coding contract in this catalog");
        catalog
            .register_complete(model_calling_registration())
            .expect("the model-calling registration is complete");
    }
}

// ===========================================================================
// Host assembly, the same shape `main.rs` uses.
// ===========================================================================

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

/// One assembled host: the server, the accepted-task queue, the prompt sender,
/// and this server's own model-call counter.
struct Harness {
    server: Server,
    queue: CodingTaskQueue,
    io: Arc<ExecIOProvider>,
    input_tx: tokio::sync::mpsc::Sender<IOMessage>,
    calls: Arc<AtomicUsize>,
}

impl Harness {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

async fn selecting_server(
    data_dir: &Path,
    composition_policy: Option<CompositionPolicy>,
    answer: &'static str,
    ceiling: Option<RunCeiling>,
) -> Harness {
    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let queue = CodingTaskQueue::new();
    let calls = Arc::new(AtomicUsize::new(0));

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    if let Some(ceiling) = ceiling {
        server.insert_global(ceiling);
    }
    let mut compositions = CompositionTemplates::new();
    compositions.register(
        MODEL_CALLING,
        CompositionTemplate::new(
            "polaris-code.recipe.test-model-call",
            &["cosmos.verified-step", "cosmos.resolve-residuals"],
            model_calling_graph,
        ),
    );
    server.insert_global(compositions);
    if let Some(policy) = composition_policy {
        server.insert_global(policy);
    }
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(CountingModelPlugin {
            calls: calls.clone(),
            answer,
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()))
        .add_plugins(TopologyCatalogPlugin)
        .add_plugins(ModelCallingPackPlugin)
        .add_plugins(TopologyEvidencePlugin::new(policy::evidence_config()))
        .add_plugins(CodingTaskQueuePlugin::new(queue.clone()));

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

    Harness {
        server,
        queue,
        io: provider,
        input_tx,
        calls,
    }
}

fn open_session(server: &Server, workspace: &Path) -> SessionId {
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = SessionId::new();
    let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
    let config = AgentConfig::new(MODEL, workspace.to_path_buf());
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

/// The session's own token total, read out of the session context.
async fn session_tokens(sessions: &SessionsAPI, session_id: &SessionId) -> u64 {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<TokenMeter>()
                .map(|meter| meter.total_tokens())
                .unwrap_or_default()
        })
        .await
        .expect("the session context is addressable")
}

/// How many canonical messages the session's transcript holds.
async fn session_messages(sessions: &SessionsAPI, session_id: &SessionId) -> usize {
    sessions
        .with_context(session_id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .map(|history| history.messages.len())
                .unwrap_or_default()
        })
        .await
        .expect("the session context is addressable")
}

// ===========================================================================
// The proof.
// ===========================================================================

#[tokio::test]
async fn a_selected_topology_reaches_the_model_and_bills_its_own_meter() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let harness = selecting_server(data_dir.path(), None, ANSWER, None).await;
    let server = &harness.server;
    let queue = &harness.queue;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(server, project_dir.path());

    // Nothing has been spent and nothing has been said.
    assert_eq!(session_tokens(sessions, &session_id).await, 0);
    assert_eq!(session_messages(sessions, &session_id).await, 0);

    // The host declares the run context will carry a model provider.
    queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;

    // (1) A topology run can reach a model.
    assert_eq!(
        outcome.reason,
        TurnReason::Completed,
        "the turn did not complete: {:?}",
        outcome.topology
    );
    let topology = outcome
        .topology
        .as_ref()
        .expect("the turn selected a topology");
    let TopologyTurn::Executed(executed) = topology else {
        panic!("expected an executed topology, got {topology:?}");
    };
    assert_eq!(executed.registration_id, MODEL_CALLING);
    assert_eq!(
        harness.calls(),
        1,
        "the graph's node really called the provider"
    );
    let after = std::fs::read_to_string(project_dir.path().join("src/lib.rs"))
        .expect("the edited file is readable");
    assert!(
        after.contains("a + b"),
        "the model-calling graph applied its edit: {after}"
    );

    // (2) The spend is on the run's own meter.
    let usage = executed
        .usage
        .as_ref()
        .expect("an executed topology reports its own accounting");
    assert_eq!(usage.input_tokens, INPUT_TOKENS);
    assert_eq!(usage.output_tokens, OUTPUT_TOKENS);
    assert_eq!(usage.total_tokens, INPUT_TOKENS + OUTPUT_TOKENS);
    assert_eq!(usage.model_id, MODEL);

    // (3) The run's spend is also folded onto the session's own meter, so the
    // operator's accounting sees it, while the turn number and transcript —
    // which belong to the ordinary conversational loop this run bypassed —
    // are unmoved.
    assert_eq!(
        session_tokens(sessions, &session_id).await,
        INPUT_TOKENS + OUTPUT_TOKENS,
        "the run's spend must reach the session's meter"
    );
    assert_eq!(
        outcome
            .usage
            .as_ref()
            .expect("the session reports its own accounting")
            .total_tokens,
        INPUT_TOKENS + OUTPUT_TOKENS
    );
    assert_eq!(outcome.turn_number, 0);
    assert_eq!(session_messages(sessions, &session_id).await, 0);
}

/// The chooser is a separate, explicitly installed policy. Its scripted
/// response names the test pack's only approved recipe; both its own call and
/// the graph's model call land in the run meter, while the Cosmos evidence
/// retains the model-policy provenance and plan identity for replay.
#[tokio::test]
async fn an_explicit_model_composition_policy_chooses_and_accounts_before_start() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let harness = selecting_server(
        data_dir.path(),
        Some(CompositionPolicy::Model),
        MODEL_CHOICE,
        None,
    )
    .await;
    let sessions = harness.server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&harness.server, project_dir.path());
    harness.queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );

    let outcome = run::drive_turn(sessions, &session_id).await;
    assert_eq!(outcome.reason, TurnReason::Completed, "{outcome:?}");
    let Some(TopologyTurn::Executed(executed)) = outcome.topology.as_ref() else {
        panic!(
            "expected model-chosen execution, got {:?}",
            outcome.topology
        );
    };
    assert_eq!(
        harness.calls(),
        2,
        "chooser then graph each call the provider"
    );
    let request = executed
        .composition_request
        .as_ref()
        .expect("the selected request is retained for replay");
    assert_eq!(request.policy(), &CompositionPolicy::Model);
    assert_eq!(request.recipe(), "polaris-code.recipe.test-model-call");
    assert!(request.prompt_digest().is_some());
    assert!(request.response_digest().is_some());
    assert!(request.grounds().contains("verified composition"));
    let usage = executed.usage.as_ref().expect("run meter is frozen");
    assert_eq!(usage.input_tokens, INPUT_TOKENS * 2);
    assert_eq!(usage.output_tokens, OUTPUT_TOKENS * 2);
    let evidence = executed.composition.as_ref().expect("canonical evidence");
    assert!(matches!(
        evidence.decision().choice().origin(),
        polaris_cosmos::contracts::CompositionChoiceOrigin::Autonomous { policy }
            if policy.id().as_str() == "polaris-code.composition-model-chooser"
    ));
    assert_eq!(
        evidence.decision().choice().plan().composition_digest(),
        executed
            .composition
            .as_ref()
            .expect("same record")
            .composition_digest(),
        "the stored plan is the no-call replay identity"
    );
    let replay = polaris_code_core::cosmos::composition::select_and_materialize_request(
        MODEL_CALLING,
        Some(CompositionTemplate::new(
            "polaris-code.recipe.test-model-call",
            &["cosmos.verified-step", "cosmos.resolve-residuals"],
            model_calling_graph,
        )),
        request.clone(),
        executed.decision_id,
        RecordedAt::new(LogicalSequence::new(999), None),
        false,
        &coding_contract_key(),
    )
    .expect("recorded typed request re-materialises without a chooser call");
    assert_eq!(harness.calls(), 2, "replay does not call the model");
    assert_eq!(
        replay.decision().composition_digest(),
        evidence.composition_digest(),
        "the recorded plan reproduces the exact composition"
    );
}

async fn assert_invalid_model_choice_refuses_before_graph(answer: &'static str) {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let harness = selecting_server(
        data_dir.path(),
        Some(CompositionPolicy::Model),
        answer,
        None,
    )
    .await;
    let sessions = harness.server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(&harness.server, project_dir.path());
    harness.queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;
    assert_ne!(
        outcome.reason,
        TurnReason::Completed,
        "invalid choice must refuse"
    );
    // A stated, recoverable refusal, not the fatal `DriverStep::Stop` an
    // ordinary chooser failure used to reach: this test pack registers
    // exactly one topology, so there is no fallback candidate to try, and
    // the turn still ends in a typed no-start answer the driver survives.
    assert_eq!(outcome.reason, TurnReason::NoTopologyStarted, "{outcome:?}");
    assert!(
        run::driver_continues(&outcome.reason),
        "an ordinary chooser failure must never end the session"
    );
    assert_eq!(
        harness.calls(),
        1,
        "no graph or fallback starts after refusal"
    );
    let source =
        std::fs::read_to_string(project_dir.path().join("src/lib.rs")).expect("source is readable");
    assert!(
        source.contains("todo!()"),
        "refusal must not edit the workspace"
    );
}

#[tokio::test]
async fn an_unknown_model_recipe_refuses_before_start_without_fallback() {
    assert_invalid_model_choice_refuses_before_graph(UNKNOWN_MODEL_CHOICE).await;
}

#[tokio::test]
async fn an_over_bound_model_configuration_refuses_before_start_without_fallback() {
    assert_invalid_model_choice_refuses_before_graph(OVER_BOUND_MODEL_CHOICE).await;
}

#[tokio::test]
async fn the_session_meter_still_moves_on_a_conversational_turn() {
    // Non-vacuity: the "unmoved" turn-number and transcript assertions above
    // are only evidence if the same session can move them, and the meter
    // assertion below shows the session's spend keeps accumulating across a
    // topology run and an ordinary conversational turn, rather than one
    // resetting the other.
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    let harness = selecting_server(data_dir.path(), None, ANSWER, None).await;
    let server = &harness.server;
    let queue = &harness.queue;
    let input_tx = harness.input_tx.clone();
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
    let topology_turn = run::drive_turn(sessions, &session_id).await;
    assert!(matches!(
        topology_turn.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    let after_topology = session_tokens(sessions, &session_id).await;
    assert_eq!(after_topology, INPUT_TOKENS + OUTPUT_TOKENS);

    // Now an ordinary turn, with nothing accepted.
    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("is anything else wrong?".to_string()),
        ))
        .await
        .expect("the prompt is enqueued before the turn starts");
    let conversational = run::drive_turn(sessions, &session_id).await;
    harness.io.close().await;

    assert_eq!(conversational.reason, TurnReason::Completed);
    assert!(conversational.topology.is_none());
    assert_eq!(
        conversational.turn_number, 1,
        "the conversational turn advances the session, so 'unmoved' means something"
    );
    assert!(
        session_tokens(sessions, &session_id).await > after_topology,
        "the session's meter keeps moving when the session itself spends too"
    );
    assert!(
        session_messages(sessions, &session_id).await > 0,
        "the session's transcript grows when the session itself talks"
    );
}

/// The end-to-end regression the accounting fix exists for: a live model call
/// made *inside* a topology run's own graph must still be visible to the
/// operator's session-wide spend ceiling, not only to the run's own reported
/// usage.
#[tokio::test]
async fn a_topology_runs_model_spend_counts_against_the_session_ceiling() {
    let (project_dir, task) = project();
    let data_dir = tempfile::tempdir().expect("temp data dir");
    // Set one token below what the single scripted call bills, so the turn
    // that makes the call is the one that trips it.
    let ceiling = RunCeiling::from_limits(None, Some(INPUT_TOKENS + OUTPUT_TOKENS - 1))
        .expect("a positive token limit is valid")
        .expect("one limit was given, so there is a ceiling");
    let harness = selecting_server(data_dir.path(), None, ANSWER, Some(ceiling)).await;
    let server = &harness.server;
    let queue = &harness.queue;
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    let session_id = open_session(server, project_dir.path());

    queue.accept(
        AcceptedCodingTask::new(task).with_host_capabilities(HostCapabilities {
            test_runner: false,
            model_provider: true,
        }),
    );
    let outcome = run::drive_turn(sessions, &session_id).await;

    // The turn itself still completed and the graph still ran — the ceiling
    // stops the *next* turn, not the one that crossed it.
    assert!(matches!(
        outcome.topology.as_ref(),
        Some(TopologyTurn::Executed(_))
    ));
    assert_eq!(
        outcome.reason,
        TurnReason::CeilingReached,
        "the session's ceiling must see a topology run's model spend: {outcome:?}"
    );
    let ceiling_status = outcome
        .ceiling
        .as_ref()
        .expect("a ceiling was installed for this session");
    assert!(
        ceiling_status.stopped,
        "the ceiling must report itself crossed"
    );
    assert_eq!(ceiling_status.spent_tokens, INPUT_TOKENS + OUTPUT_TOKENS);
}

/// The contract the test pack claims is the shipped coding contract, not a
/// look-alike. If this drifts, the two tests above would be proving a property
/// of some other contract.
#[test]
fn the_test_pack_claims_the_shipped_coding_contract() {
    assert_eq!(coding_contract_key().id().as_str(), topology::CONTRACT);
    assert!(
        topology::contract()
            .expect("the coding contract builds")
            .signature()
            .satisfies(&GraphSignature::new().produce::<AssessmentConclusion>())
    );
}
