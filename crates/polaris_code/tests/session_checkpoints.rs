//! Consumer proof for durable, identity-stable, bounded session checkpoints.
//!
//! The host is the real coding composition: [`CodeAgent`], tools, persistence,
//! the ontrace store, a filesystem [`FileStore`], and the production headless
//! IO adapter. Only the model provider is scripted, so the proof is offline and
//! deterministic while still exercising the coding agent's graph and context.

use std::path::Path;
use std::sync::Arc;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IOContent, IOMessage, IOProvider, PersistenceAPI, PersistenceError, PersistencePlugin,
    ResourceSerializer,
};
use polaris::sessions::checkpoint::{CheckpointOrigin, CheckpointRetention, CheckpointStatus};
use polaris::sessions::{
    AgentTypeId, FileStore, SessionError, SessionId, SessionsAPI, SessionsPlugin,
};
use polaris::system::param::SystemContext;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::resource::LocalResource;
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

const ANSWER: &str = "checkpoint proof response";

/// The only fake in the composition: one deterministic, network-free model.
struct ScriptedProvider;

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "checkpoint-scripted"
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
        Ok(futures_util::stream::iter(vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ANSWER.to_owned()),
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
    const ID: &'static str = "test::checkpoint_scripted_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin builds before the scripted provider")
            .register_llm_provider(ScriptedProvider);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Probe {
    value: String,
}

impl Probe {
    fn new(value: &str) -> Self {
        Self {
            value: value.to_owned(),
        }
    }
}

impl LocalResource for Probe {}

/// A movable-version serializer used to prove both restore modes after a
/// schema bump. The coding agent's graph remains otherwise unchanged.
struct ProbeSerializer {
    version: &'static str,
}

impl ProbeSerializer {
    fn at(version: &'static str) -> Arc<dyn ResourceSerializer> {
        Arc::new(Self { version })
    }
}

impl ResourceSerializer for ProbeSerializer {
    fn plugin_id(&self) -> &'static str {
        "consumer-proof"
    }

    fn storage_key(&self) -> &'static str {
        "CheckpointProbe"
    }

    fn schema_version(&self) -> &'static str {
        self.version
    }

    fn save(&self, ctx: &SystemContext<'_>) -> Result<Option<Value>, PersistenceError> {
        if !ctx.contains_local_resource::<Probe>() {
            return Ok(None);
        }
        let probe = ctx
            .get_resource::<Probe>()
            .map_err(|err| PersistenceError::ResourceAccess(err.to_string()))?;
        Ok(Some(serde_json::to_value(&*probe)?))
    }

    fn load(&self, value: Value, ctx: &mut SystemContext<'_>) -> Result<(), PersistenceError> {
        ctx.insert(serde_json::from_value::<Probe>(value)?);
        Ok(())
    }
}

struct Host {
    server: Server,
    provider: Arc<ExecIOProvider>,
    input: mpsc::Sender<IOMessage>,
    drain: tokio::task::JoinHandle<usize>,
}

impl Host {
    fn sessions(&self) -> &SessionsAPI {
        self.server
            .api::<SessionsAPI>()
            .expect("SessionsAPI is installed")
    }

    async fn turn(&self, id: &SessionId, prompt: &str) -> polaris::graph::ExecutionResult {
        self.input
            .send(IOMessage::from_external(
                "checkpoint-proof",
                IOContent::Text(prompt.to_owned()),
            ))
            .await
            .expect("the coding input channel stays open");
        self.sessions()
            .process_turn(id)
            .await
            .expect("the real coding turn completes")
    }

    async fn shutdown(self) {
        self.provider.close().await;
        self.drain.await.expect("output drain joins");
    }
}

async fn build_host(
    data_dir: &Path,
    retention: Option<CheckpointRetention>,
    auto_checkpoint: bool,
    serializers: Option<Vec<Arc<dyn ResourceSerializer>>>,
) -> Host {
    let (provider, input, mut output) = ExecIOProvider::new(128);
    let provider = Arc::new(provider);

    let mut sessions_plugin = SessionsPlugin::new(Arc::new(FileStore::new(data_dir)));
    if let Some(retention) = retention {
        sessions_plugin = sessions_plugin.with_checkpoint_retention(retention);
    }
    if !auto_checkpoint {
        sessions_plugin = sessions_plugin.without_auto_checkpoint();
    }

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin)
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(sessions_plugin)
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(Arc::clone(&provider)));
    server.finish().await.expect("the coding host finishes");

    let sessions = server.api::<SessionsAPI>().unwrap();
    if let Some(serializers) = serializers {
        sessions.set_serializers(serializers);
    } else {
        let persistence = server.api::<PersistenceAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
    }
    sessions.register_agent(CodeAgent).unwrap();

    let drain = tokio::spawn(async move {
        let mut count = 0;
        while output.recv().await.is_some() {
            count += 1;
        }
        count
    });

    Host {
        server,
        provider,
        input,
        drain,
    }
}

fn executor() -> polaris::graph::GraphExecutor {
    polaris::graph::GraphExecutor::new().with_default_max_iterations(25)
}

fn config(workspace: &Path) -> AgentConfig {
    AgentConfig::new("checkpoint-scripted/model", workspace.to_path_buf())
}

fn create_session(host: &Host, id: &SessionId, config: &AgentConfig, probe: Option<Probe>) {
    host.sessions()
        .create_session_with_executor(
            host.server.create_context(),
            id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            executor(),
            {
                let config = config.clone();
                move |ctx| {
                    ctx.insert(config.clone());
                    if let Some(probe) = probe.clone() {
                        ctx.insert(probe);
                    }
                }
            },
        )
        .expect("the coding session is created");
}

async fn resume_session(host: &Host, id: &SessionId, config: &AgentConfig, probe: Option<Probe>) {
    host.sessions()
        .resume_session_with_executor(host.server.create_context(), id, executor(), {
            let config = config.clone();
            move |ctx| {
                ctx.insert(config.clone());
                if let Some(probe) = probe.clone() {
                    ctx.insert(probe);
                }
            }
        })
        .await
        .expect("the coding session resumes");
}

async fn probe_bytes(sessions: &SessionsAPI, id: &SessionId) -> Vec<u8> {
    sessions
        .with_context(id, |ctx| {
            serde_json::to_vec(&*ctx.get_resource::<Probe>().unwrap()).unwrap()
        })
        .await
        .unwrap()
}

/// Assertion 1: the default-on coding host creates U0 plus one distinct point
/// per turn, with correct resume numbering and discoverable retention bounds.
#[tokio::test]
async fn assertion_1_default_checkpoints_have_stable_identity_and_resume_numbers() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let host = build_host(data_dir.path(), None, true, None).await;
    let id = SessionId::new();
    create_session(&host, &id, &config(workspace.path()), None);

    for prompt in ["first", "second", "third"] {
        host.turn(&id, prompt).await;
    }

    let checkpoints = host.sessions().checkpoints();
    let points = checkpoints.list(&id).unwrap();
    assert_eq!(points.len(), 4, "creation plus three completed turns");
    assert_eq!(
        points
            .iter()
            .map(|point| point.resume_turn)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert!(
        points
            .windows(2)
            .all(|pair| pair[0].id.as_u64() < pair[1].id.as_u64()),
        "checkpoint ids are distinct and strictly increasing"
    );

    let policy = checkpoints.retention();
    let usage = checkpoints.usage(&id).unwrap();
    assert_eq!(usage.max_count, policy.max_count);
    assert_eq!(usage.max_bytes, policy.max_bytes);
    assert_eq!(usage.count, points.len());
    assert!(usage.bytes > 0);

    host.shutdown().await;
}

/// Assertion 2: restore executes turn 1 next and mints a fresh run id instead
/// of repeating the earlier turn-1 invocation.
#[tokio::test]
async fn assertion_2_restore_fixes_the_default_paths_run_identity() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let host = build_host(data_dir.path(), None, true, None).await;
    let id = SessionId::new();
    create_session(&host, &id, &config(workspace.path()), None);

    let mut original_runs = Vec::new();
    for prompt in ["first", "second", "third"] {
        original_runs.push(host.turn(&id, prompt).await.run_id().clone());
    }
    let target = host
        .sessions()
        .checkpoints()
        .list(&id)
        .unwrap()
        .into_iter()
        .find(|point| point.resume_turn == 1)
        .unwrap();

    let restored = host
        .sessions()
        .checkpoints()
        .restore(&id, target.id)
        .await
        .unwrap();
    assert_eq!(restored.resume_turn, 1);
    assert_eq!(host.sessions().session_info(&id).unwrap().turn_number, 1);

    let rerun = host.turn(&id, "branch turn one").await;
    assert_eq!(host.sessions().session_info(&id).unwrap().turn_number, 2);
    assert_ne!(
        rerun.run_id(),
        &original_runs[1],
        "the second execution of logical turn 1 has a fresh run identity"
    );

    host.shutdown().await;
}

/// Assertion 3: restoring twice opens two revisions while preserving and
/// classifying the abandoned branch.
#[tokio::test]
async fn assertion_3_repeat_restore_preserves_the_abandoned_branch() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let host = build_host(data_dir.path(), None, true, None).await;
    let id = SessionId::new();
    create_session(&host, &id, &config(workspace.path()), None);
    for prompt in ["first", "second", "third"] {
        host.turn(&id, prompt).await;
    }

    let checkpoints = host.sessions().checkpoints();
    let initial = checkpoints.list(&id).unwrap();
    let target = initial
        .iter()
        .find(|point| point.resume_turn == 1)
        .unwrap()
        .clone();
    let abandoned: Vec<_> = initial
        .iter()
        .filter(|point| point.resume_turn > 1)
        .map(|point| point.id)
        .collect();

    let first = checkpoints.restore(&id, target.id).await.unwrap();
    host.turn(&id, "new branch").await;
    let second = checkpoints.restore(&id, target.id).await.unwrap();
    assert!(second.revision > first.revision);
    assert_eq!(second.resume_turn, 1);

    let listed = checkpoints.list(&id).unwrap();
    for abandoned_id in abandoned {
        assert!(listed.iter().any(|point| point.id == abandoned_id));
        assert!(matches!(
            checkpoints.status(&id, abandoned_id).unwrap(),
            CheckpointStatus::OffHistory(_)
        ));
    }

    host.shutdown().await;
}

/// Assertion 4: the coding host's existing save call makes U1 durable across
/// a real server drop/rebuild/resume, with no checkpoint-specific host code.
#[tokio::test]
async fn assertion_4_file_store_round_trip_preserves_and_restores_points() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let config = config(workspace.path());

    let before = {
        let host = build_host(data_dir.path(), None, true, None).await;
        create_session(&host, &id, &config, None);
        for prompt in ["first", "second", "third"] {
            host.turn(&id, prompt).await;
        }
        let before = host.sessions().checkpoints().list(&id).unwrap();
        host.sessions().save_session(&id).await.unwrap();
        host.shutdown().await;
        before
    };

    let host = build_host(data_dir.path(), None, true, None).await;
    resume_session(&host, &id, &config, None).await;
    let after = host.sessions().checkpoints().list(&id).unwrap();
    for expected in &before {
        let actual = after
            .iter()
            .find(|point| point.id == expected.id)
            .expect("every pre-restart identity survives");
        assert_eq!(actual.resume_turn, expected.resume_turn);
    }

    let target = before.iter().find(|point| point.resume_turn == 1).unwrap();
    let outcome = host
        .sessions()
        .checkpoints()
        .restore(&id, target.id)
        .await
        .expect("a restored checkpoint is usable");
    assert_eq!(outcome.resume_turn, 1);

    host.shutdown().await;
}

/// Assertion 5: a post-resume capture allocates above every restored identity,
/// including the automatically added resume-boundary point.
#[tokio::test]
async fn assertion_5_identity_allocation_remains_unique_after_restart() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let config = config(workspace.path());

    {
        let host = build_host(data_dir.path(), None, true, None).await;
        create_session(&host, &id, &config, None);
        host.turn(&id, "before restart").await;
        host.sessions().save_session(&id).await.unwrap();
        host.shutdown().await;
    }

    let host = build_host(data_dir.path(), None, true, None).await;
    resume_session(&host, &id, &config, None).await;
    let restored = host.sessions().checkpoints().list(&id).unwrap();
    let highest = restored
        .iter()
        .map(|point| point.id.as_u64())
        .max()
        .unwrap();
    let fresh = host
        .sessions()
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    assert!(fresh.id.as_u64() > highest);

    host.shutdown().await;
}

/// Assertion 6: a bumped serializer is reported by permissive restore, while
/// exact restore refuses without changing even one byte of the live resource.
#[tokio::test]
async fn assertion_6_schema_bump_reports_partial_and_exact_is_byte_atomic() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let config = config(workspace.path());

    let target = {
        let host = build_host(
            data_dir.path(),
            None,
            true,
            Some(vec![ProbeSerializer::at("1.0.0")]),
        )
        .await;
        create_session(&host, &id, &config, Some(Probe::new("checkpoint-version")));
        host.turn(&id, "capture version one").await;
        let target = host.sessions().checkpoints().current(&id).unwrap().unwrap();
        host.sessions()
            .with_context(&id, |ctx| {
                ctx.get_resource_mut::<Probe>().unwrap().value = "stored-live".to_owned();
            })
            .await
            .unwrap();
        host.sessions().save_session(&id).await.unwrap();
        host.shutdown().await;
        target
    };

    let host = build_host(
        data_dir.path(),
        None,
        true,
        Some(vec![ProbeSerializer::at("2.0.0")]),
    )
    .await;
    resume_session(&host, &id, &config, Some(Probe::new("before-refusal"))).await;

    let permissive = host
        .sessions()
        .checkpoints()
        .restore(&id, target.id)
        .await
        .expect("permissive restore reports the mismatch");
    assert!(!permissive.unmatched.is_empty());
    assert_eq!(permissive.unmatched[0].version, "1.0.0");

    let before = probe_bytes(host.sessions(), &id).await;
    let error = host
        .sessions()
        .checkpoints()
        .restore_exact(&id, target.id)
        .await
        .expect_err("exact restore refuses the old schema");
    assert!(matches!(error, SessionError::RestoreIncomplete { .. }));
    let after = probe_bytes(host.sessions(), &id).await;
    assert_eq!(
        after, before,
        "a refused exact restore leaves the context byte-identical"
    );

    host.shutdown().await;
}

/// Assertion 7: both crash cuts leave a parseable file and only complete
/// checkpoint records; an unsaved capture never leaks into the stored prefix.
#[tokio::test]
async fn assertion_7_file_store_crash_cuts_keep_a_complete_prefix() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let config = config(workspace.path());
    let session_file = data_dir.path().join(format!("{}.json", id.as_str()));

    {
        let host = build_host(data_dir.path(), None, true, None).await;
        create_session(&host, &id, &config, None);
        host.sessions().save_session(&id).await.unwrap();
        host.sessions()
            .checkpoints()
            .capture(&id, CheckpointOrigin::Manual)
            .await
            .unwrap();
        host.shutdown().await;
    }

    let first_cut: Value = serde_json::from_slice(&tokio::fs::read(&session_file).await.unwrap())
        .expect("capture-before-save leaves the prior file parseable");
    assert_eq!(first_cut["checkpoints"].as_array().unwrap().len(), 1);

    let saved_after_second_cut = {
        let host = build_host(data_dir.path(), None, true, None).await;
        resume_session(&host, &id, &config, None).await;
        host.sessions()
            .checkpoints()
            .capture(&id, CheckpointOrigin::Manual)
            .await
            .unwrap();
        host.sessions().save_session(&id).await.unwrap();
        let saved = host.sessions().checkpoints().list(&id).unwrap();
        host.shutdown().await;
        saved
    };

    let second_cut: Value = serde_json::from_slice(&tokio::fs::read(&session_file).await.unwrap())
        .expect("save-before-next-turn leaves the replacement file parseable");
    for checkpoint in second_cut["checkpoints"].as_array().unwrap() {
        assert!(checkpoint.get("meta").is_some());
        assert!(checkpoint.get("resources").is_some());
    }

    let host = build_host(data_dir.path(), None, true, None).await;
    resume_session(&host, &id, &config, None).await;
    let resumed = host.sessions().checkpoints().list(&id).unwrap();
    for saved in saved_after_second_cut {
        assert!(resumed.iter().any(|point| point.id == saved.id));
    }
    host.shutdown().await;
}

/// Assertion 8: if the mandatory pre-restore capture cannot fit because the
/// sole retained point is pinned as current/target, restore changes nothing.
#[tokio::test]
async fn assertion_8_failed_pre_restore_capture_leaves_context_unchanged() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let host = build_host(
        data_dir.path(),
        Some(CheckpointRetention::new(Some(1), None)),
        false,
        Some(vec![ProbeSerializer::at("1.0.0")]),
    )
    .await;
    let id = SessionId::new();
    create_session(
        &host,
        &id,
        &config(workspace.path()),
        Some(Probe::new("captured")),
    );
    let target = host
        .sessions()
        .checkpoints()
        .capture(&id, CheckpointOrigin::Manual)
        .await
        .unwrap();
    host.sessions()
        .with_context(&id, |ctx| {
            ctx.get_resource_mut::<Probe>().unwrap().value = "must-survive".to_owned();
        })
        .await
        .unwrap();
    let before = probe_bytes(host.sessions(), &id).await;

    let error = host
        .sessions()
        .checkpoints()
        .restore(&id, target.id)
        .await
        .expect_err("the pre-restore capture has no evictable slot");
    assert!(matches!(error, SessionError::CheckpointCapacity { .. }));
    assert_eq!(probe_bytes(host.sessions(), &id).await, before);
    assert_eq!(host.sessions().checkpoints().usage(&id).unwrap().count, 1);

    host.shutdown().await;
}
