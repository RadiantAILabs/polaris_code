//! Consumer proof for the embedding entry point documented in
//! `src/docs/embedding.md`.
//!
//! Everything below is written the way a program that is *not* the
//! `polaris-code` binary has to write it: `polaris_code_core::` paths only, no
//! access to `main.rs`'s private composition helpers, and no `ANTHROPIC_API_KEY`
//! — the embedder supplies its own [`LlmProvider`] the same way `main.rs`
//! supplies `AnthropicPlugin`.
//!
//! What it proves, in the order the guide presents it:
//!
//! 1. the six things an embedder must supply are enough to drive a turn to
//!    completion against a working directory, and the turn really acts on that
//!    directory (`read_file` returns the file the consumer wrote there);
//! 2. the run's accounted usage comes back on the outcome, in the published
//!    `polaris-code-run-usage/v1` shape, keyed to the session's own model id;
//! 3. a permission confirmation is answered on the embedder's own IO channel —
//!    the constraint the guide names as the likeliest runtime surprise;
//! 4. [`run::request_interrupt`] stops a turn in flight from outside the turn,
//!    the outcome is [`TurnReason::Interrupted`], and the usage the run had
//!    already accrued is still reported;
//! 5. [`run::request_interrupt`] answers `false` for an idle session and for
//!    one that does not exist.
//!
//! Every item the guide names is used here, so an item that stops being public
//! fails this test at compile time.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
    LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    CONFIRMED, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOMessage, IOProvider,
    PersistenceAPI, PersistencePlugin,
};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::tools::{CodeToolsConfig, CodeToolsPlugin};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::{Notify, mpsc};

/// The provider name the embedder registers under; the session's model id must
/// carry it as its `provider/model` prefix or `CodeAgent::setup` cannot resolve
/// a model at all.
const PROVIDER: &str = "embedded";

/// The model id this consumer configures its session with.
const MODEL_ID: &str = "embedded/test-model";

/// One scripted model reply, consumed in order by [`ScriptedProvider::stream`].
enum Reply {
    /// Stream one tool call, then stop with `ToolUse`.
    ToolCall {
        name: &'static str,
        arguments: serde_json::Value,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Stream one text answer, then stop with `EndTurn`.
    Text {
        text: String,
        input_tokens: u64,
        output_tokens: u64,
    },
    /// Signal `entered`, then never yield again — the turn can only end by
    /// being interrupted.
    Stall { entered: Arc<Notify> },
}

/// An [`LlmProvider`] that plays a fixed script.
///
/// State lives in the instance rather than in statics, so each test builds its
/// own server, its own [`ModelRegistry`] and its own script, and the tests do
/// not interfere when the harness runs them in parallel.
#[derive(Clone)]
struct ScriptedProvider {
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

impl ScriptedProvider {
    fn new(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Arc::new(Mutex::new(replies.into_iter().collect())),
        }
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        // `CodeAgent` streams; a non-streaming call means the graph's error
        // handler served the turn, which the assertions below must not accept
        // as the real step.
        unreachable!("the embedded CodeAgent uses streaming generation")
    }

    async fn stream(&self, _: &str, _: LlmRequest) -> Result<LlmStream, GenerationError> {
        let reply = self
            .replies
            .lock()
            .expect("script lock poisoned")
            .pop_front()
            .expect("the script ran out of replies");
        let stream: LlmStream = match reply {
            Reply::ToolCall {
                name,
                arguments,
                input_tokens,
                output_tokens,
            } => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: format!("{name}-1"),
                        call_id: None,
                        name: name.to_string(),
                    },
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: arguments.to_string(),
                    },
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: billed(input_tokens, output_tokens),
                    id: None,
                    model: None,
                }),
            ])
            .boxed(),
            Reply::Text {
                text,
                input_tokens,
                output_tokens,
            } => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text(text),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: billed(input_tokens, output_tokens),
                    id: None,
                    model: None,
                }),
            ])
            .boxed(),
            Reply::Stall { entered } => futures_util::stream::once(async move {
                entered.notify_one();
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                })
            })
            .chain(futures_util::stream::pending())
            .boxed(),
        };
        Ok(stream)
    }
}

/// The cumulative usage one scripted reply reports on its `MessageStop`.
fn billed(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens: Some(input_tokens),
        output_tokens: Some(output_tokens),
        total_tokens: Some(input_tokens + output_tokens),
        ..Default::default()
    }
}

/// Registers the embedder's own model provider — the seam `AnthropicPlugin`
/// occupies in the shipped binary.
struct ScriptedModelPlugin {
    provider: ScriptedProvider,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::embedded_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build first")
            .register_llm_provider(self.provider.clone());
    }
}

/// Test scaffolding that owns what an embedder owns — not a shipped type, and
/// deliberately not a wrapper over the run surface: it holds the handles the
/// documented steps hand back and nothing else.
struct Embedder {
    /// Kept alive because it owns the APIs and the running plugins.
    _server: Server,
    sessions: SessionsAPI,
    session_id: SessionId,
    input: mpsc::Sender<IOMessage>,
    output: mpsc::Receiver<IOMessage>,
    provider: Arc<ExecIOProvider>,
}

/// The documented start-up sequence, executed through public API only.
///
/// Mirrors `src/docs/embedding.md` step for step: supply IO, supply a model,
/// install the capability plugins, install the meter as a global *before* any
/// context is created, finish, register the agent, create the session with its
/// [`AgentConfig`], and overwrite the policy `CodeAgent::setup` seeded.
async fn embed(
    working_dir: &std::path::Path,
    script: Vec<Reply>,
    mode: PermissionMode,
) -> Embedder {
    let (io, input, output) = ExecIOProvider::new(64);
    let provider = Arc::new(io);

    let mut server = Server::new();
    // Before `create_context()`. This is not optional: `act` takes the meter as
    // a required `Res<TokenMeter>`, so without it the step cannot run and the
    // turn comes back "completed" with no answer and no accounting at all.
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin {
            provider: ScriptedProvider::new(script),
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(working_dir)))
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.finish().await.expect("the embedded server starts");

    let persistence = server
        .api::<PersistenceAPI>()
        .expect("PersistencePlugin provides PersistenceAPI");
    let sessions = server
        .api::<SessionsAPI>()
        .expect("SessionsPlugin provides SessionsAPI")
        .clone();
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("the agent registers once");

    let session_id = SessionId::new();
    let config = AgentConfig::new(MODEL_ID, working_dir.to_path_buf());
    sessions
        .create_session_with(
            server.create_context(),
            &session_id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .expect("the session is created");

    // After creation, never before: `CodeAgent::setup` seeds
    // `PolicySnapshot::defaults()`, whose permission policy is empty, so a
    // posture chosen here has to overwrite it.
    let mut policy = PolicySnapshot::defaults();
    policy.permission = mode.permission_policy();
    sessions
        .with_context(&session_id, move |ctx| {
            ctx.insert(policy);
        })
        .await
        .expect("the session accepts its permission policy");

    Embedder {
        _server: server,
        sessions,
        session_id,
        input,
        output,
        provider,
    }
}

/// Enqueue one prompt before the turn starts — `receive_user_input` swallows a
/// closed channel rather than erroring, so a send racing the turn can hang.
async fn prompt(input: &mpsc::Sender<IOMessage>, text: &str) {
    input
        .send(IOMessage::from_external(
            "embedder",
            IOContent::Text(text.to_string()),
        ))
        .await
        .expect("the input channel accepts the prompt");
}

/// Drain whatever the turn published and return the `io_type`s in order.
fn drained_io_types(output: &mut mpsc::Receiver<IOMessage>) -> Vec<String> {
    let mut seen = Vec::new();
    while let Ok(message) = output.try_recv() {
        if let Some(io_type) = message.metadata.get(IO_TYPE) {
            seen.push(io_type.clone());
        }
    }
    seen
}

#[tokio::test]
async fn an_embedder_drives_a_turn_against_its_working_directory_and_reads_the_usage() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    std::fs::write(workspace.path().join("NOTES.md"), "ship the entry point\n")
        .expect("the consumer seeds its own working directory");

    let mut host = embed(
        workspace.path(),
        vec![
            Reply::ToolCall {
                name: "read_file",
                arguments: serde_json::json!({ "path": "NOTES.md" }),
                input_tokens: 7,
                output_tokens: 3,
            },
            Reply::Text {
                text: "NOTES.md says: ship the entry point".into(),
                input_tokens: 11,
                output_tokens: 5,
            },
        ],
        // Nothing in this turn needs a grant; the posture is stated anyway,
        // because leaving it unstated is what the guide warns against.
        PermissionMode::Unattended(UnattendedGrants::default()),
    )
    .await;

    prompt(&host.input, "what does NOTES.md say?").await;
    let outcome = run::drive_turn(&host.sessions, &host.session_id).await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(outcome.reason.exit_code(), 0);
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("NOTES.md says: ship the entry point"),
        "the embedder reads the turn's answer off the outcome, not off the IO stream"
    );
    assert!(outcome.run_id.is_some(), "an executed turn has a run id");
    assert!(outcome.nodes_executed > 0);
    assert_eq!(outcome.turn_number, 1);
    assert!(
        outcome.topology.is_none(),
        "an embedder that accepted no coding task gets the conversational turn"
    );

    // The tool really ran against the directory the embedder configured: the
    // second model call only happens because the first produced a tool result.
    let io_types = drained_io_types(&mut host.output);
    assert!(
        io_types.iter().any(|io_type| io_type == "tool"),
        "expected a tool block in {io_types:?}"
    );

    let usage = outcome.usage.expect("a metered run reports its accounting");
    assert_eq!(usage.schema, "polaris-code-run-usage/v1");
    assert_eq!(
        usage.model_id, MODEL_ID,
        "the model id is the session's own, not the embedder's copy"
    );
    assert_eq!(usage.input_tokens, 18, "7 + 11 across the two act steps");
    assert_eq!(usage.output_tokens, 8, "3 + 5 across the two act steps");
    assert_eq!(usage.total_tokens, 26);
    assert_eq!(
        usage.total_tokens,
        usage.input_tokens
            + usage.output_tokens
            + usage.cache_read_tokens
            + usage.cache_creation_tokens
    );
    assert!(
        !usage.pricing_basis.is_empty(),
        "unpriced is still attributed"
    );
    assert!(
        usage.estimated_cost_usd.is_none(),
        "a fixture model is in no price table: null means unpriced, never free"
    );

    host.provider.close().await;
}

#[tokio::test]
async fn an_embedder_answers_a_permission_confirmation_on_its_own_io_channel() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let file = workspace.path().join("NOTES.md");
    std::fs::write(&file, "before\n").expect("the consumer seeds its working directory");

    let mut host = embed(
        workspace.path(),
        vec![
            Reply::ToolCall {
                name: "edit_file",
                arguments: serde_json::json!({
                    "path": "NOTES.md",
                    "old_string": "before",
                    "new_string": "after",
                    "replace_all": false,
                }),
                input_tokens: 4,
                output_tokens: 2,
            },
            Reply::Text {
                text: "edited".into(),
                input_tokens: 4,
                output_tokens: 2,
            },
        ],
        // Confirm is the interactive posture: `edit_file` is registered
        // `ToolPermission::Confirm`, so this turn *will* ask.
        PermissionMode::Confirm,
    )
    .await;

    prompt(&host.input, "change before to after in NOTES.md").await;

    // Answer the question the turn is about to ask. The answer goes on the
    // same input channel the prompt did, because `UserIO::confirm` sends the
    // question and then blocks on `receive()`.
    let approve = host.input.clone();
    let approver = tokio::spawn(async move {
        approve
            .send(
                IOMessage::system_text("approved")
                    .with_metadata(CONFIRMED, CONFIRMED_TRUE.to_string()),
            )
            .await
    });

    let outcome = run::drive_turn(&host.sessions, &host.session_id).await;
    approver.await.expect("the approver task finishes").ok();

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(
        std::fs::read_to_string(&file).expect("the edited file is readable"),
        "after\n",
        "the approved edit reached the embedder's working directory"
    );
    let io_types = drained_io_types(&mut host.output);
    assert!(
        io_types.iter().any(|io_type| io_type == IO_TYPE_CONFIRM),
        "the embedder must see the question it answered, in {io_types:?}"
    );

    host.provider.close().await;
}

#[tokio::test]
async fn an_embedder_interrupts_a_running_turn_and_still_reads_what_it_spent() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let entered = Arc::new(Notify::new());

    let host = embed(
        workspace.path(),
        vec![
            Reply::Text {
                text: "first answer".into(),
                input_tokens: 9,
                output_tokens: 6,
            },
            Reply::Stall {
                entered: entered.clone(),
            },
        ],
        PermissionMode::Unattended(UnattendedGrants::default()),
    )
    .await;

    // Spend something first, so "usage is still reported" is a claim about a
    // non-zero number rather than about an empty meter.
    prompt(&host.input, "say something").await;
    let first = run::drive_turn(&host.sessions, &host.session_id).await;
    assert_eq!(first.reason, TurnReason::Completed);
    assert_eq!(
        first
            .usage
            .as_ref()
            .expect("the first turn is metered")
            .total_tokens,
        15
    );

    prompt(&host.input, "now stall").await;
    let turn_sessions = host.sessions.clone();
    let turn_id = host.session_id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&turn_sessions, &turn_id).await });

    entered.notified().await;
    assert!(
        run::request_interrupt(&host.sessions, &host.session_id),
        "a turn in flight reports that it was asked to stop"
    );

    let outcome = turn.await.expect("the driving task finishes");
    assert_eq!(outcome.reason, TurnReason::Interrupted);
    assert_eq!(outcome.reason.exit_code(), 130);
    assert!(
        outcome.final_text.is_none(),
        "an interrupted turn has no answer"
    );
    assert_eq!(
        outcome.turn_number, 2,
        "a turn that was started and then interrupted is still a turn the \
         session counted; the next prompt gets turn 3, not a retry of this one"
    );

    let usage = outcome
        .usage
        .expect("an interrupted run still reports what it spent");
    assert_eq!(usage.model_id, MODEL_ID);
    assert_eq!(
        usage.total_tokens, 15,
        "the meter carries the completed turn's spend into the interrupted one"
    );

    host.provider.close().await;
}

#[tokio::test]
async fn request_interrupt_is_false_when_there_is_nothing_to_interrupt() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let host = embed(
        workspace.path(),
        vec![],
        PermissionMode::Unattended(UnattendedGrants::default()),
    )
    .await;

    assert!(
        !run::request_interrupt(&host.sessions, &host.session_id),
        "an idle session has no turn to stop"
    );
    assert!(
        !run::request_interrupt(&host.sessions, &SessionId::new()),
        "an unknown session is reported as nothing to stop, not as an error"
    );

    host.provider.close().await;
}
