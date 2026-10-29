//! Regression proof that a session-control command ends an idle agent turn.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
    LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{PersistencePlugin, UserIO};
use polaris::sessions::{
    AgentTypeId, InMemoryStore, SessionData, SessionId, SessionStore, SessionsAPI, SessionsPlugin,
};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::ExecIOProvider;
use polaris_code::run;
use polaris_code::terminal::{CommandDispatch, CommandRegistry, SessionAction, SessionNavigation};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::commands::CommandInvocation;

#[derive(Default)]
struct ModelState {
    calls: AtomicUsize,
}

struct BoundaryProvider(Arc<ModelState>);

impl LlmProvider for BoundaryProvider {
    fn name(&self) -> &'static str {
        "session-boundary-test"
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("CodeAgent streams")
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        Ok(futures_util::stream::iter([
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("orphan answer".into()),
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

struct BoundaryModelPlugin(Arc<ModelState>);

impl Plugin for BoundaryModelPlugin {
    const ID: &'static str = "test::session_boundary_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .unwrap()
            .register_llm_provider(BoundaryProvider(self.0.clone()));
    }
}

struct BoundaryIoPlugin(Arc<CommandDispatch<ExecIOProvider>>);

impl Plugin for BoundaryIoPlugin {
    const ID: &'static str = "test::session_boundary_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let provider = self.0.clone();
        server.register_local(move || UserIO::new(provider.clone()));
    }
}

/// Everything a boundary test drives: one live session, the navigation its
/// commands schedule into, and the channel a `/command` arrives on.
struct Boundary {
    model: Arc<ModelState>,
    sessions: SessionsAPI,
    navigation: SessionNavigation,
    input: tokio::sync::mpsc::Sender<polaris::plugins::IOMessage>,
    current: SessionId,
    /// Held open for the test's lifetime: dropping the receiver closes the
    /// channel the dispatcher answers a `/command` on, and a failed reply is
    /// indistinguishable from no command at all — the turn reaches the model.
    _output: tokio::sync::mpsc::Receiver<polaris::plugins::IOMessage>,
    _workspace: tempfile::TempDir,
}

/// Build a server whose only live session is `current`, over `store`.
async fn boundary(store: Arc<InMemoryStore>) -> Boundary {
    let workspace = tempfile::tempdir().unwrap();
    let model = Arc::new(ModelState::default());
    let (provider, input, output) = ExecIOProvider::new(64);

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(BoundaryModelPlugin(model.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(store.clone()))
        .add_plugins(CodeAgentPlugin::new());

    let commands = Arc::new(CommandRegistry::with_builtins());
    let dispatch = CommandDispatch::new(
        Arc::new(provider),
        commands.clone(),
        workspace.path().into(),
    );
    server.add_plugins(BoundaryIoPlugin(Arc::new(dispatch)));
    server.finish().await.unwrap();

    let sessions = server.api::<SessionsAPI>().unwrap().clone();
    let current = SessionId::from_string("current");
    let navigation = SessionNavigation::new(sessions.clone(), current.clone())
        .with_store(store.clone() as Arc<dyn SessionStore>);
    navigation.register(&commands).unwrap();
    sessions.register_agent(CodeAgent).unwrap();
    let config = AgentConfig::new("session-boundary-test/model", workspace.path());
    sessions
        .create_session_with(
            server.create_context(),
            &current,
            &AgentTypeId::from_name(CodeAgent::NAME),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .unwrap();

    Boundary {
        model,
        sessions,
        navigation,
        input,
        current,
        _output: output,
        _workspace: workspace,
    }
}

/// A picker choice is a new way to *say* which session, not a second way to
/// move the active one: it lands in the same pending slot `/new` uses and is
/// still the composition root's to apply after the turn ends.
#[tokio::test]
async fn a_picker_choice_takes_effect_only_after_the_turn_it_was_made_in() {
    let store = Arc::new(InMemoryStore::new());
    store
        .save(
            &SessionId::from_string("yesterday"),
            &SessionData::new("CodeAgent", 4, "2026-09-20T09:00:00Z", Vec::new()),
        )
        .await
        .unwrap();
    let harness = boundary(store).await;

    harness
        .input
        .send(CommandInvocation::new("resume", "1").into_message("/resume 1"))
        .await
        .unwrap();
    let outcome = run::drive_turn(&harness.sessions, &harness.current).await;

    assert_eq!(
        harness.model.calls.load(Ordering::SeqCst),
        0,
        "choosing a session must not run the model"
    );
    assert_eq!(outcome.reason, run::TurnReason::Completed);
    assert_eq!(
        harness
            .sessions
            .session_info(&harness.current)
            .unwrap()
            .turn_number,
        0,
        "a choice is an input boundary, not a canonical agent turn"
    );
    assert_eq!(
        harness.navigation.current().as_str(),
        "current",
        "the switch has not happened inside the turn the choice was made in"
    );
    assert_eq!(
        harness.navigation.take_pending(),
        Some(SessionAction::Resume(SessionId::from_string("yesterday"))),
        "it is waiting in the one slot the composition root drains at the boundary"
    );
}

#[tokio::test]
async fn session_control_ends_turn_without_appending_an_assistant_message() {
    let harness = boundary(Arc::new(InMemoryStore::new())).await;
    let Boundary {
        model,
        sessions,
        navigation,
        input,
        current,
        ..
    } = &harness;

    input
        .send(CommandInvocation::new("new", "next").into_message("/new next"))
        .await
        .unwrap();
    let outcome = run::drive_turn(sessions, current).await;

    let history = sessions
        .with_context(current, |ctx| {
            ctx.get_resource::<ContextManager>()
                .unwrap()
                .messages
                .clone()
        })
        .await
        .unwrap();
    assert_eq!(model.calls.load(Ordering::SeqCst), 0, "model must not run");
    assert_eq!(outcome.reason, run::TurnReason::Completed);
    assert_eq!(
        sessions.session_info(current).unwrap().turn_number,
        0,
        "host control is an input boundary, not a canonical agent turn"
    );
    assert!(
        history
            .iter()
            .all(|message| !matches!(message, Message::Assistant { .. })),
        "a control-only turn must not append assistant history: {history:?}"
    );
    assert_eq!(
        navigation.take_pending(),
        Some(SessionAction::New(SessionId::from_string("next"))),
        "the composition root must still receive the requested switch"
    );

    // The host-boundary path returns before the outcome match, so it is the one
    // exit that could silently skip the meter. `None` would claim the turn was
    // unmeasured, which is false here: the session has a TokenMeter and an
    // AgentConfig, and the turn simply spent nothing because no model ran.
    let usage = outcome
        .usage
        .expect("a control-only turn is measured, not unmeasured");
    assert_eq!(
        usage.model_id, "session-boundary-test/model",
        "usage must name the model the session is configured with"
    );
    assert_eq!(
        usage.total_tokens, 0,
        "no model ran, so the reported spend is a true zero: {usage:?}"
    );
}
