//! Consumer proofs for the interactive host's rewind composition.
//!
//! These tests use the native session/checkpoint handle, real `CodeAgent`
//! context, command registry, durable edit journal, ontrace store, and the
//! production [`RewindApplication`]. Only the model and terminal transport are
//! headless. This keeps the assertions on the same seam `main.rs` calls after
//! a graph turn releases its context lock.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IOContent, IOMessage, PersistenceAPI, PersistenceError, PersistencePlugin, ResourceSerializer,
};
use polaris::sessions::checkpoint::CheckpointOrigin;
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::param::SystemContext;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::resource::LocalResource;
use polaris::system::server::Server;
use polaris::tools::{ToolRegistry, ToolsPlugin};
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, LoopState, PermissionMode,
    PolicySnapshot,
};
use polaris_code_core::config::ModelCatalog;
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::terminal::{
    AttachmentStore, BackgroundTasks, CommandOutcome, CommandRegistry, EditJournal,
    RewindApplication, RewindControl, RewindMode, RewindRequest, SessionSettings, TurnSteering,
};
use polaris_code_core::tools::{CodeGraphTools, CodeIndexer, CodeToolsConfig};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use polaris_tui::commands::CommandInvocation;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MODEL: &str = "rewind-scripted/model";

struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "rewind-scripted"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("done")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        Ok(futures_util::stream::iter(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("done".into()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage::default(),
                id: None,
                model: None,
            }),
        ])
        .boxed())
    }
}

struct ScriptedModelPlugin;

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::terminal_rewind_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(ScriptedProvider);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Probe(String);
impl LocalResource for Probe {}

struct ProbeSerializer(&'static str);

impl ResourceSerializer for ProbeSerializer {
    fn plugin_id(&self) -> &'static str {
        "rewind-proof"
    }
    fn storage_key(&self) -> &'static str {
        "RewindProbe"
    }
    fn schema_version(&self) -> &'static str {
        self.0
    }
    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError> {
        if !ctx.contains_local_resource::<Probe>() {
            return Ok(None);
        }
        Ok(Some(serde_json::to_value(
            &*ctx
                .get_resource::<Probe>()
                .map_err(|error| PersistenceError::ResourceAccess(error.to_string()))?,
        )?))
    }
    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError> {
        ctx.insert(serde_json::from_value::<Probe>(value)?);
        Ok(())
    }
}

struct Host {
    server: Server,
    sessions: SessionsAPI,
    session: SessionId,
    journal: EditJournal,
    commands: CommandRegistry,
    application: RewindApplication,
    input: tokio::sync::mpsc::Sender<IOMessage>,
    _provider: Arc<ExecIOProvider>,
    serializers: Vec<Arc<dyn ResourceSerializer>>,
    working_dir: PathBuf,
    _root: tempfile::TempDir,
}

impl Host {
    async fn boot(with_probe: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let working_dir = root.path().join("repo");
        std::fs::create_dir_all(&working_dir).unwrap();
        let working_dir = working_dir.canonicalize().unwrap();
        let (provider, input, mut output) = ExecIOProvider::new(64);
        let provider = Arc::new(provider);
        tokio::spawn(async move { while output.recv().await.is_some() {} });

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin)
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                working_dir.join(".polaris/sessions"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ExecIOPlugin::new(provider.clone()));
        server.finish().await.unwrap();

        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        let mut serializers = server.api::<PersistenceAPI>().unwrap().serializers();
        if with_probe {
            serializers.push(Arc::new(ProbeSerializer("1")));
        }
        sessions.set_serializers(serializers.clone());
        sessions.register_agent(CodeAgent).unwrap();

        let config = AgentConfig::new(MODEL, &working_dir);
        let settings =
            SessionSettings::new(MODEL, PermissionMode::Confirm, &ModelCatalog::pinned(MODEL));
        let steering = TurnSteering::enabled();
        let attachments = AttachmentStore::new();
        let (tasks, _) = BackgroundTasks::new(Duration::from_secs(60));
        let policy = PolicySnapshot::defaults();
        let session = SessionId::from_string("rewind-proof");
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(8),
                {
                    let config = config.clone();
                    let settings = settings.clone();
                    let steering = steering.clone();
                    let attachments = attachments.clone();
                    let tasks = tasks.clone();
                    let policy = policy.clone();
                    move |ctx| {
                        ctx.insert(config.clone());
                        ctx.insert(settings.clone());
                        ctx.insert(steering.clone());
                        ctx.insert(attachments.clone());
                        ctx.insert(tasks.clone());
                        ctx.insert(policy.clone());
                        if with_probe {
                            ctx.insert(Probe("target".into()));
                        }
                    }
                },
            )
            .unwrap();

        let journal = EditJournal::open(&working_dir).unwrap();
        let commands = CommandRegistry::with_builtins();
        let current = session.clone();
        let control =
            RewindControl::new(sessions.clone(), journal.clone(), move || current.clone());
        control.register(&commands).unwrap();
        let store = server
            .get_global::<OxigraphStore>()
            .map(|store| OxigraphStore::clone(&store));
        let application = RewindApplication::new(
            sessions.clone(),
            journal.clone(),
            config,
            settings,
            steering,
            attachments,
            tasks,
            policy,
            store,
            working_dir.clone(),
        );
        Self {
            server,
            sessions,
            session,
            journal,
            commands,
            application,
            input,
            _provider: provider,
            serializers,
            working_dir,
            _root: root,
        }
    }

    async fn point(&self, label: &str) -> polaris_code_core::terminal::RewindPoint {
        let checkpoint = self
            .sessions
            .checkpoints()
            .capture(&self.session, CheckpointOrigin::Manual)
            .await
            .unwrap();
        let revision = self.sessions.checkpoints().revision(&self.session).unwrap();
        self.journal
            .add_point(&self.session, revision, checkpoint.id, label.into())
            .unwrap()
    }

    async fn probe(&self) -> Vec<u8> {
        self.sessions
            .with_context(&self.session, |ctx| {
                serde_json::to_vec(&*ctx.get_resource::<Probe>().unwrap()).unwrap()
            })
            .await
            .unwrap()
    }
}

fn request(point: polaris_code_core::terminal::RewindPoint, mode: RewindMode) -> RewindRequest {
    RewindRequest { point, mode }
}

#[tokio::test]
async fn exact_restore_refuses_an_unmatched_serializer_before_mutation() {
    let host = Host::boot(true).await;
    let point = host.point("exact target").await;
    host.sessions
        .with_context(&host.session, |ctx| {
            ctx.get_resource_mut::<Probe>().unwrap().0 = "live".into();
        })
        .await
        .unwrap();
    let before = host.probe().await;
    let serializers = host
        .serializers
        .iter()
        .filter(|serializer| serializer.storage_key() != "RewindProbe")
        .cloned()
        .chain(std::iter::once(
            Arc::new(ProbeSerializer("2")) as Arc<dyn ResourceSerializer>
        ))
        .collect();
    host.sessions.set_serializers(serializers);

    let outcome = host
        .application
        .apply(&host.session, &request(point, RewindMode::Conversation))
        .await;
    assert!(outcome.conversation.contains("refused/failed"));
    assert_eq!(host.probe().await, before, "refusal is byte-atomic");
}

#[tokio::test]
async fn preview_mutates_nothing_and_only_confirmed_dispatch_queues_application() {
    let host = Host::boot(false).await;
    let point = host.point("before edit").await;
    let file = host.working_dir.join("tracked.txt");
    std::fs::write(&file, b"before").unwrap();
    host.journal
        .record_edit(&file, b"before".to_vec(), b"after".to_vec())
        .unwrap();
    std::fs::write(&file, b"after").unwrap();

    let preview = host
        .commands
        .dispatch(&CommandInvocation::new(
            "rewind",
            format!("{} edits", point.id),
        ))
        .await;
    assert!(matches!(preview, CommandOutcome::Reply(_)));
    assert_eq!(std::fs::read(&file).unwrap(), b"after");

    let confirmed = host
        .commands
        .dispatch(&CommandInvocation::new(
            "rewind",
            format!("{} edits confirm", point.id),
        ))
        .await;
    assert!(matches!(confirmed, CommandOutcome::Control(_)));
    // Dispatch only queues: the quiescent host owns application.
    assert_eq!(std::fs::read(&file).unwrap(), b"after");
}

#[tokio::test]
async fn application_waits_for_the_real_turn_and_preserves_the_recovery_route() {
    let host = Host::boot(false).await;
    let point = host.point("before turn").await;
    let file = host.working_dir.join("turn.rs");
    std::fs::write(&file, b"fn before() {}\n").unwrap();
    host.journal
        .record_edit(
            &file,
            b"fn before() {}\n".to_vec(),
            b"fn after() {}\n".to_vec(),
        )
        .unwrap();
    std::fs::write(&file, b"fn after() {}\n").unwrap();

    let sessions = host.sessions.clone();
    let session = host.session.clone();
    let turn = tokio::spawn(async move { sessions.process_turn(&session).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let app = host.application.clone();
    let session = host.session.clone();
    let request = request(point.clone(), RewindMode::Both);
    let apply = tokio::spawn(async move { app.apply(&session, &request).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !apply.is_finished(),
        "the in-flight graph owns the context lock"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"fn after() {}\n");

    host.input
        .send(IOMessage::from_external(
            "rewind-proof",
            IOContent::Text("finish the turn".into()),
        ))
        .await
        .unwrap();
    turn.await.unwrap().unwrap();
    let outcome = apply.await.unwrap();
    assert_eq!(outcome.conversation, "restored exactly");
    assert_eq!(std::fs::read(&file).unwrap(), b"fn before() {}\n");
    let recovery = host
        .sessions
        .checkpoints()
        .list(&host.session)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.id.as_u64() > point.checkpoint.as_u64())
        .expect("restore captured the state it left");
    host.sessions
        .checkpoints()
        .restore_exact(&host.session, recovery.id)
        .await
        .expect("the pre-restore recovery route remains reachable");
}

#[tokio::test]
async fn restore_reconciles_runtime_messages_loop_state_and_code_index() {
    let host = Host::boot(false).await;
    let file = host.working_dir.join("indexed.rs");
    std::fs::write(&file, b"fn restored_name() {}\n").unwrap();
    CodeIndexer::index_repo(
        &host.server.get_global::<OxigraphStore>().unwrap(),
        &host.working_dir,
    )
    .await
    .unwrap();
    host.sessions
        .with_context(&host.session, |ctx| {
            ctx.get_resource_mut::<ContextManager>()
                .unwrap()
                .push(polaris::models::llm::Message::user("restored message"));
            ctx.get_resource_mut::<LoopState>().unwrap().is_complete = true;
        })
        .await
        .unwrap();
    let point = host.point("runtime target").await;
    host.journal
        .record_edit(
            &file,
            b"fn restored_name() {}\n".to_vec(),
            b"fn later_name() {}\n".to_vec(),
        )
        .unwrap();
    std::fs::write(&file, b"fn later_name() {}\n").unwrap();
    CodeIndexer::index_paths(
        &host.server.get_global::<OxigraphStore>().unwrap(),
        &host.working_dir,
        std::slice::from_ref(&file),
    )
    .await
    .unwrap();
    host.sessions
        .with_context(&host.session, |ctx| {
            ctx.get_resource_mut::<ContextManager>()
                .unwrap()
                .push(polaris::models::llm::Message::user("discard me"));
            ctx.get_resource_mut::<LoopState>().unwrap().is_complete = false;
        })
        .await
        .unwrap();

    let outcome = host
        .application
        .apply(&host.session, &request(point, RewindMode::Both))
        .await;
    assert_eq!(outcome.conversation, "restored exactly");
    assert_eq!(outcome.index, "refreshed");
    host.sessions
        .with_context(&host.session, |ctx| {
            let history = ctx.get_resource::<ContextManager>().unwrap();
            let rendered = format!("{:?}", history.messages);
            assert!(rendered.contains("restored message"));
            assert!(!rendered.contains("discard me"));
            assert!(
                history
                    .project(&PolicySnapshot::defaults().retrieval)
                    .cache_prefix,
                "post-restore reconciliation reinstates the configured cache-aware strategy"
            );
            assert!(!ctx.get_resource::<LoopState>().unwrap().is_complete);
        })
        .await
        .unwrap();

    let store = host.server.get_global::<OxigraphStore>().unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeGraphTools::new(
        OxigraphStore::clone(&store),
        CodeToolsConfig::new(&host.working_dir),
    ));
    let found = registry
        .execute(
            "find_definition",
            &serde_json::json!({"name": "restored_name"}),
        )
        .await
        .unwrap();
    assert!(found.to_string().contains("indexed.rs"), "{found:?}");
}

#[tokio::test]
async fn a_partial_file_failure_is_retryable_before_conversation_restore() {
    let host = Host::boot(false).await;
    let point = host.point("partial target").await;
    let first = host.working_dir.join("a.txt");
    let second = host.working_dir.join("b.txt");
    for path in [&first, &second] {
        std::fs::write(path, b"before").unwrap();
        host.journal
            .record_edit(path, b"before".to_vec(), b"after".to_vec())
            .unwrap();
        std::fs::write(path, b"after").unwrap();
    }
    let blocker = second.with_extension(format!("polaris-rewind-{}", std::process::id()));
    std::fs::create_dir(&blocker).unwrap();
    let request = request(point, RewindMode::Both);
    let failed = host.application.apply(&host.session, &request).await;
    assert!(failed.conversation.contains("not attempted"));
    assert_eq!(std::fs::read(&first).unwrap(), b"before");
    assert_eq!(std::fs::read(&second).unwrap(), b"after");

    std::fs::remove_dir(&blocker).unwrap();
    let recovered = host.application.apply(&host.session, &request).await;
    assert_eq!(recovered.conversation, "restored exactly");
    assert_eq!(std::fs::read(&second).unwrap(), b"before");
}
