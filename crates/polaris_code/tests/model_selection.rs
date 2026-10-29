//! Proof that the operator's model choice reaches the provider, and that the
//! run's own records name the model that served the turn.
//!
//! Every assertion here is against something the run produced — the model
//! string the provider was actually called with, and
//! [`RunUsage::model_id`](polaris_code::usage::RunUsage) — never against
//! the argument that was passed in. The launch path is reproduced the way
//! `main.rs` composes it: resolve through [`ModelCatalog`], restrict to the
//! registered providers, then seed one [`AgentConfig`] and one
//! [`SessionSettings`] from the resolved id.

use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin, UserIO};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode};
use polaris_code::config::{ModelCatalog, ModelSelectionError};
use polaris_code::exec::ExecIOProvider;
use polaris_code::run;
use polaris_code::terminal::{CommandDispatch, CommandRegistry, SessionSettings};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::commands::CommandInvocation;
use tokio::sync::mpsc;

/// The default this host is configured with.
const STARTUP_MODEL: &str = "recording/alpha";
/// A second reachable model the operator may name instead.
const OTHER_MODEL: &str = "recording/beta";
/// A model the operator may declare but no registered provider serves.
const UNREACHABLE_MODEL: &str = "absent/gamma";
/// How long a driven turn may take before the test calls it wedged.
const TURN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Every model string the provider was asked to serve, in call order.
#[derive(Default)]
struct Served(Mutex<Vec<String>>);

impl Served {
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

/// An `LlmProvider` whose only job is to remember which model it was called
/// with. That record — not the configuration that produced it — is what the
/// assertions below read.
struct RecordingProvider(Arc<Served>);

impl LlmProvider for RecordingProvider {
    fn name(&self) -> &'static str {
        "recording"
    }

    /// The agent streams; a non-streaming call means the turn fell into
    /// recovery. Recorded with a marker rather than panicking, so that shows
    /// up as a failed assertion naming the path instead of a dead task.
    async fn generate(&self, model: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        self.0.0.lock().unwrap().push(format!("recover:{model}"));
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("recovered")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, model: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.0.0.lock().unwrap().push(model.to_string());
        Ok(futures_util::stream::iter([
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

struct RecordingModelPlugin(Arc<Served>);

impl Plugin for RecordingModelPlugin {
    const ID: &'static str = "test::model_selection_provider";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the recording provider")
            .register_llm_provider(RecordingProvider(self.0.clone()));
    }
}

/// Installs the command-executing `UserIO` decorator the interactive front end
/// uses, so `/model` is run locally and never reaches the model.
struct DispatchIoPlugin(Arc<CommandDispatch<ExecIOProvider>>);

impl Plugin for DispatchIoPlugin {
    const ID: &'static str = "test::model_selection_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let provider = self.0.clone();
        server.register_local(move || UserIO::new(provider.clone()));
    }
}

/// One host wired the way `main.rs` wires the launch path.
struct Host {
    server: Server,
    commands: Arc<CommandRegistry>,
    settings: SessionSettings,
    session: SessionId,
    input: mpsc::Sender<IOMessage>,
}

impl Host {
    /// Boot a host whose declared set is `declared`, launched with `requested`
    /// (the `--model` argument, or `None` for the configured default).
    ///
    /// Returns the resolution error instead of a host when the name could not
    /// be resolved — which is exactly what `main.rs` turns into an exit-2
    /// startup refusal, before any provider exists to be called.
    async fn launch(
        workspace: &Path,
        data_dir: &Path,
        declared: &[&str],
        requested: Option<&str>,
        served: Arc<Served>,
    ) -> Result<Self, ModelSelectionError> {
        // Step one, before the server exists: the declared set and the name.
        let declared_models = ModelCatalog::new(STARTUP_MODEL, declared.to_vec());
        let model_id = declared_models.select(requested)?;

        // The output channel is bounded and `receive_user_input` swallows a
        // closed one, so a front end that stops draining wedges the turn.
        // Drain it the way both real front ends do.
        let (io, input, mut output) = ExecIOProvider::new(64);
        tokio::spawn(async move { while output.recv().await.is_some() {} });
        let commands = Arc::new(CommandRegistry::with_builtins());
        let dispatch = CommandDispatch::new(Arc::new(io), commands.clone(), workspace.into());

        let mut server = Server::new();
        server.insert_global(TokenMeter::new());
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(RecordingModelPlugin(served.clone()))
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data_dir.to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(DispatchIoPlugin(Arc::new(dispatch)));
        server.finish().await.unwrap();

        // Step two, once the registry exists: narrow the set to what is
        // actually reachable, and re-resolve the startup model against it.
        let catalog = {
            let models = server.get_global::<ModelRegistry>().unwrap();
            declared_models.restricted_to_providers(&models.llm_provider_names())
        };
        catalog.resolve(&model_id)?;

        let settings = SessionSettings::new(&model_id, PermissionMode::Confirm, &catalog);
        settings.register(&commands).unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let session = SessionId::from_string("model-selection");
        let config = AgentConfig::new(&model_id, workspace);
        let settings_seed = settings.clone();
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
                move |ctx| {
                    ctx.insert(config.clone());
                    ctx.insert(settings_seed.clone());
                },
            )
            .unwrap();

        Ok(Self {
            server,
            commands,
            settings,
            session,
            input,
        })
    }

    fn sessions(&self) -> SessionsAPI {
        self.server.api::<SessionsAPI>().unwrap().clone()
    }

    /// Submit one prompt and run the turn it starts.
    ///
    /// Bounded so that a turn which never reaches the model fails the test
    /// with a message rather than hanging the suite.
    async fn turn(&self, prompt: &str) -> run::TurnOutcome {
        self.input
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.to_string()),
            ))
            .await
            .unwrap();
        tokio::time::timeout(
            TURN_DEADLINE,
            run::drive_turn(&self.sessions(), &self.session),
        )
        .await
        .expect("the turn did not reach the model within its bound")
    }

    /// The `/model` picker's choices, as the composer popup would list them.
    fn model_choices(&self) -> Vec<String> {
        self.commands
            .specs()
            .into_iter()
            .find(|spec| spec.name == "model")
            .expect("`/model` is registered")
            .choices
    }
}

#[tokio::test]
async fn a_model_named_at_launch_is_the_model_that_serves_the_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let served = Arc::new(Served::default());

    let host = Host::launch(
        workspace.path(),
        data_dir.path(),
        &[STARTUP_MODEL, OTHER_MODEL],
        Some(OTHER_MODEL),
        served.clone(),
    )
    .await
    .expect("a declared, reachable model resolves");

    let outcome = host.turn("say something").await;
    assert_eq!(outcome.reason, run::TurnReason::Completed);

    // What the provider was handed, not what the host was told to use.
    assert_eq!(
        served.calls(),
        ["beta"],
        "the named model must be the one the provider served"
    );
    // And what the run recorded about itself.
    let usage = outcome.usage.expect("a measured turn");
    assert_eq!(
        usage.model_id, OTHER_MODEL,
        "the usage record must name the model that served the turn"
    );
}

#[tokio::test]
async fn selecting_in_session_moves_the_next_turn_and_its_record() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let served = Arc::new(Served::default());

    let host = Host::launch(
        workspace.path(),
        data_dir.path(),
        &[STARTUP_MODEL, OTHER_MODEL],
        None,
        served.clone(),
    )
    .await
    .expect("the configured default resolves");

    assert!(
        host.model_choices().len() > 1,
        "a host reaching two models must offer two: {:?}",
        host.model_choices()
    );
    assert_eq!(host.model_choices(), [STARTUP_MODEL, OTHER_MODEL]);

    let first = host.turn("before").await;
    assert_eq!(
        first.usage.expect("a measured turn").model_id,
        STARTUP_MODEL
    );

    host.commands
        .dispatch(&CommandInvocation::new("model", OTHER_MODEL))
        .await;
    assert_eq!(host.settings.snapshot().model_id, OTHER_MODEL);

    let second = host.turn("after").await;
    assert_eq!(
        served.calls(),
        ["alpha", "beta"],
        "the second turn must run on the selected model"
    );
    assert_eq!(
        second.usage.expect("a measured turn").model_id,
        OTHER_MODEL,
        "the record must follow the turn, not the launch configuration"
    );
}

#[tokio::test]
async fn an_unknown_model_is_refused_at_launch_before_any_provider_exists() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let served = Arc::new(Served::default());

    let error = Host::launch(
        workspace.path(),
        data_dir.path(),
        &[STARTUP_MODEL, OTHER_MODEL],
        Some("recording/does-not-exist"),
        served.clone(),
    )
    .await
    .err()
    .expect("an undeclared model must not launch");

    let message = error.to_string();
    assert!(message.contains("recording/does-not-exist"), "{message}");
    assert!(message.contains(STARTUP_MODEL), "{message}");
    assert!(message.contains(OTHER_MODEL), "{message}");
    assert!(
        served.calls().is_empty(),
        "the refusal must land before any provider call: {:?}",
        served.calls()
    );
}

#[tokio::test]
async fn a_declared_model_with_no_registered_provider_is_refused_at_launch() {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let served = Arc::new(Served::default());

    let error = Host::launch(
        workspace.path(),
        data_dir.path(),
        &[STARTUP_MODEL, UNREACHABLE_MODEL],
        Some(UNREACHABLE_MODEL),
        served.clone(),
    )
    .await
    .err()
    .expect("a model whose provider is not registered is not reachable");

    let message = error.to_string();
    assert!(message.contains("absent"), "{message}");
    assert!(message.contains(STARTUP_MODEL), "{message}");
    assert!(
        served.calls().is_empty(),
        "the refusal must land before any provider call: {:?}",
        served.calls()
    );
}
