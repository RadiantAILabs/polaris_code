//! Proof that an `@` mention naming a path outside the bound working directory
//! never reaches the model as prompt text — on *every* path a prompt can be
//! submitted by, not just the one the check happened to be wired into.
//!
//! The containment check lives in `CommandDispatch`, a decorator around
//! the `UserIO` channel. A host that installs a
//! [`TurnInputHandle`](polaris_tui::panels::conversation::TurnInputHandle) —
//! which the interactive front end does, because turn steering needs one —
//! submits into that queue instead and never crosses the decorator. These
//! tests drive the queue, because that is the path the refusal had to learn.
//!
//! The edges are the only fakes: a capturing [`LlmProvider`] stands in for
//! `AnthropicPlugin`, so there is no key and no network, and the panel is
//! driven with [`TuiEvent`]s instead of a crossterm loop. Everything between
//! is production code — the real composer, the real steering queue, the real
//! `receive_user_input`, the real turn driver. What is asserted at the far end
//! is the request the provider was handed, because the one thing that must
//! never happen is `@../secrets` arriving as text the model can act on.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode};
use polaris_code::config::ModelCatalog;
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::terminal::{self, CommandRegistry, SessionSettings, TurnSteering};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiEvent, TuiPanel, TuiRegistry};

/// A model id in the registry's `provider/model` form, served by the
/// capturing provider below.
const MODEL: &str = "scripted/claude-sonnet-4-6";

const USAGE: Usage = Usage {
    input_tokens: Some(100),
    output_tokens: Some(10),
    total_tokens: Some(110),
    cache_read_tokens: None,
    cache_creation_tokens: None,
    reasoning_output_tokens: None,
};

// ─────────────────────────────────────────────────────────────────────────────
// The capturing model
// ─────────────────────────────────────────────────────────────────────────────

/// Answers every turn identically and keeps every request it was handed.
#[derive(Default, Clone)]
struct CapturingProvider {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl LlmProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.seen.lock().unwrap().push(request.messages.clone());
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("seen")],
            usage: USAGE,
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        self.seen.lock().unwrap().push(request.messages.clone());
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("seen".to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: USAGE,
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct CapturingModelPlugin {
    provider: CapturingProvider,
}

impl Plugin for CapturingModelPlugin {
    const ID: &'static str = "test::mention_containment_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the capturing model")
            .register_llm_provider(self.provider.clone());
    }
}

/// Provides the [`TuiRegistry`] capability the way `TuiPlugin::build` does and
/// nothing else, so the contributed panel stays in the registry for the test.
struct HeadlessTuiPlugin;

impl Plugin for HeadlessTuiPlugin {
    const ID: &'static str = "test::headless_tui_mentions";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }
}

/// Every user prompt the model was handed, as flat text, across all requests.
fn prompts_seen(seen: &Arc<Mutex<Vec<Vec<Message>>>>) -> Vec<String> {
    seen.lock()
        .unwrap()
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            _ => None,
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// The interactive host, wired as `main.rs` wires it
// ─────────────────────────────────────────────────────────────────────────────

struct Host {
    panel: Box<dyn TuiPanel>,
    sessions: SessionsAPI,
    session_id: SessionId,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    terminal: Terminal<TestBackend>,
    steering: TurnSteering,
}

impl Host {
    /// Boot an interactive host over `working_dir`: a `TurnInputHandle` on the
    /// panel and the steering queue behind it, exactly as the binary's
    /// interactive branch composes them.
    async fn boot(working_dir: &Path) -> Self {
        let working_dir = working_dir.canonicalize().unwrap();
        let provider = CapturingProvider::default();
        let seen = provider.seen.clone();
        let config = AgentConfig::new(MODEL, working_dir.clone());
        let commands = Arc::new(CommandRegistry::with_builtins());
        let steering = TurnSteering::enabled();

        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(CapturingModelPlugin { provider })
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                working_dir.join(".polaris/sessions"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(HeadlessTuiPlugin);

        let panel_plugin =
            ConversationPanelPlugin::default().with_turn_input_handle(Arc::new(steering.clone()));
        let (conversation, _output) =
            terminal::attach_with_output(panel_plugin, commands.clone(), &working_dir);
        server.add_plugins(conversation);
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let panel = server
            .api::<TuiRegistry>()
            .unwrap()
            .drain()
            .into_iter()
            .find(|panel| panel.id() == "conversation")
            .expect("the conversation panel contributed itself");

        let settings =
            SessionSettings::new(MODEL, PermissionMode::Confirm, &ModelCatalog::pinned(MODEL));

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let session_id = SessionId::from_string("mentions");
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(8),
                {
                    let config = config.clone();
                    let settings = settings.clone();
                    let steering = steering.clone();
                    move |ctx| {
                        ctx.insert(config.clone());
                        ctx.insert(settings.clone());
                        ctx.insert(steering.clone());
                    }
                },
            )
            .unwrap();

        Self {
            panel,
            sessions,
            session_id,
            seen,
            terminal: Terminal::new(TestBackend::new(120, 24)).unwrap(),
            steering,
        }
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.panel
            .handle_event(&TuiEvent::Key(KeyEvent::new(code, modifiers)));
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// Let the runtime run, tick the panel and draw. Returns the screen.
    async fn frame(&mut self) -> String {
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        self.panel.tick();
        let panel = &mut self.panel;
        self.terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = self.terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Type a draft and press Enter, exactly as a user does. Returns the
    /// screen after the submission was accepted or refused.
    async fn compose_and_send(&mut self, draft: &str) -> String {
        self.type_text(draft);
        let _ = self.frame().await;
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        self.frame().await
    }

    /// Run one turn to completion, or abandon it if no prompt ever arrives —
    /// which is the shape a refusal has: nothing was consumed, so the turn
    /// never starts.
    async fn turn(&mut self) {
        let sessions = self.sessions.clone();
        let id = self.session_id.clone();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let driver = tokio::spawn(async move {
            run::drive_turn(&sessions, &id).await;
            flag.store(true, Ordering::SeqCst);
        });
        for _ in 0..400 {
            if done.load(Ordering::SeqCst) {
                break;
            }
            self.frame().await;
        }
        driver.abort();
        let _ = driver.await;
    }

    fn model_was_called(&self) -> bool {
        !self.seen.lock().unwrap().is_empty()
    }

    /// Start one real turn and leave it parked at the production input boundary.
    fn start_turn(&self) -> tokio::task::JoinHandle<()> {
        let sessions = self.sessions.clone();
        let id = self.session_id.clone();
        tokio::spawn(async move {
            run::drive_turn(&sessions, &id).await;
        })
    }
}

/// A workspace to bind the agent to, with a real secret one level *above* it.
///
/// The workspace is a subdirectory of the temp root rather than the root
/// itself, so `@../secrets.txt` resolves to a file this test owns instead of
/// whatever happens to sit beside the system temp directory. The secret
/// exists on purpose: a refusal that only worked on missing paths would be no
/// refusal at all.
///
/// Returns the root (kept alive to hold the directory open) and the working
/// directory to bind.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let working_dir = root.path().join("repo");
    std::fs::create_dir_all(working_dir.join("src")).unwrap();
    std::fs::write(working_dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(working_dir.join("notes.txt"), "plain text").unwrap();
    std::fs::write(root.path().join("secrets.txt"), "token=hunter2").unwrap();
    (root, working_dir)
}

// ─────────────────────────────────────────────────────────────────────────────
// The steering path — the one the check was missing from
// ─────────────────────────────────────────────────────────────────────────────

/// Slash-command discovery and dispatch stay live while the agent is parked on
/// its steering queue. This is the production composition: the real panel has
/// a `TurnInputHandle`, the real terminal decorator owns the command registry,
/// and the real agent turn waits for input. Only the model is fake, so proving
/// it was never called also proves `/help` stayed a host-local action.
#[tokio::test]
async fn commands_are_advertised_and_dispatched_with_steering_installed() {
    let (_root, working_dir) = workspace();
    let mut host = Host::boot(&working_dir).await;
    let driver = host.start_turn();

    host.type_text("/");
    let mut screen = String::new();
    for _ in 0..100 {
        screen = host.frame().await;
        if screen.contains("List the available commands") {
            break;
        }
    }
    assert!(
        screen.contains("List the available commands"),
        "the host command set is advertised while steering owns prompt input: {screen}"
    );

    // Finish the literal command rather than accepting the highlighted popup
    // entry, so this also proves the panel recognizes and dispatches it.
    host.type_text("help");
    host.key(KeyCode::Enter, KeyModifiers::NONE);
    for _ in 0..100 {
        screen = host.frame().await;
        if screen.contains("/help ❯") {
            break;
        }
    }
    assert!(
        screen.contains("/help ❯"),
        "the dispatched command is answered through the host: {screen}"
    );
    assert!(
        !host.model_was_called(),
        "a slash command is never submitted as model-bound prompt text"
    );

    driver.abort();
    let _ = driver.await;
}

/// THE PROOF. With a `TurnInputHandle` installed, a prompt naming a path
/// outside the working directory is refused at the composer and the model is
/// never called at all.
///
/// Before the fix this test fails twice over: the submission is accepted into
/// the steering queue, `receive_user_input` takes the steering branch and
/// pushes the text into history with no containment check, and the capturing
/// provider is handed `read @../secrets.txt and summarise it` verbatim.
#[tokio::test]
async fn an_escaping_mention_never_reaches_the_model_on_the_steering_path() {
    let (_root, working_dir) = workspace();
    let mut host = Host::boot(&working_dir).await;

    let screen = host
        .compose_and_send("read @../secrets.txt and summarise it")
        .await;

    assert!(
        screen.contains("was not sent"),
        "the host refuses the submission in words that reach the user: {screen}"
    );
    assert!(
        screen.contains("outside the working directory"),
        "and says why: {screen}"
    );
    assert!(
        !screen.contains("queued for run"),
        "a refused prompt is not in the queue, so it carries no queued badge: {screen}"
    );

    host.turn().await;
    assert!(
        !host.model_was_called(),
        "no request was made at all, so no escaping path reached a model: {:?}",
        prompts_seen(&host.seen)
    );
}

/// The refusal is actionable: the draft survives it, so the user can fix the
/// mention instead of retyping the prompt.
///
/// Proven by correcting only the offending path — the rest of the draft is
/// still in the composer for the correction to land in — and watching that
/// same prompt reach the model.
#[tokio::test]
async fn a_refused_submission_keeps_the_draft_for_correction() {
    let (_root, working_dir) = workspace();
    let mut host = Host::boot(&working_dir).await;

    host.compose_and_send("summarise @../secrets.txt").await;

    // `../secrets.txt` is 14 characters. Delete exactly those and name a file
    // inside the workspace instead; everything before the `@` must still be
    // there for this to produce a whole prompt.
    for _ in 0.."../secrets.txt".len() {
        host.key(KeyCode::Backspace, KeyModifiers::NONE);
    }
    let screen = host.compose_and_send("notes.txt").await;
    assert!(
        screen.contains("queued for run"),
        "the corrected prompt is accepted: {screen}"
    );

    host.turn().await;
    assert_eq!(
        prompts_seen(&host.seen),
        ["summarise @notes.txt".to_string()],
        "the surviving draft carried the user's own words into the corrected prompt"
    );
}

/// A mention that stays inside the workspace still reaches the model, text
/// untouched. The gate refuses escapes, not mentions.
#[tokio::test]
async fn a_contained_mention_still_reaches_the_model_on_the_steering_path() {
    let (_root, working_dir) = workspace();
    let mut host = Host::boot(&working_dir).await;

    let screen = host.compose_and_send("explain @src/main.rs").await;
    assert!(
        !screen.contains("was not sent"),
        "nothing inside the workspace is refused: {screen}"
    );

    host.turn().await;
    assert_eq!(
        prompts_seen(&host.seen),
        ["explain @src/main.rs".to_string()],
        "the prompt reaches the model exactly as typed"
    );
}

/// Every way into the turn-input queue is gated, not just the one the panel
/// happens to call today.
///
/// [`TurnInputHandle`] has three mutators that put user text where a turn will
/// read it: `submit`, `submit_with_attachments`, and `replace`. A queue that
/// checked only the first would still deliver an escape through the other two,
/// and `replace` in particular is reachable by any host without touching the
/// composer at all.
#[tokio::test]
async fn every_turn_input_ingress_refuses_an_escape() {
    let (_root, working_dir) = workspace();
    let host = Host::boot(&working_dir).await;
    let steering: &dyn TurnInputHandle = &host.steering;

    let refused = steering
        .submit("read @../secrets.txt".into(), TurnInputMode::Queue)
        .expect_err("submit refuses an escaping mention");
    assert!(
        refused.contains("was not sent") && refused.contains("outside the working directory"),
        "and says why, in the reason the panel shows: {refused}"
    );

    let refused = steering
        .submit_with_attachments(
            "read @../secrets.txt".into(),
            TurnInputMode::Queue,
            Vec::new(),
        )
        .expect_err("submit_with_attachments refuses it too");
    assert!(refused.contains("was not sent"), "{refused}");

    // `replace` rewrites text already accepted into the queue, so it has to
    // re-check: a contained prompt must not become an escaping one in place.
    let receipt = steering
        .submit("explain @src/main.rs".into(), TurnInputMode::Queue)
        .expect("a contained mention is accepted");
    let refused = steering
        .replace(receipt.id, "read @../secrets.txt".into())
        .expect_err("replace refuses an escaping rewrite");
    assert!(refused.contains("was not sent"), "{refused}");
    steering
        .replace(receipt.id, "explain @notes.txt".into())
        .expect("a contained rewrite is accepted");
}

/// Mid-turn steering is the second way the queue feeds the model: text drained
/// at a safe boundary by `apply_steering`, not by `receive_user_input`. It
/// reads the same queue, so the ingress gate covers it — and because nothing
/// unchecked can be in the queue, there is no second check to forget.
#[tokio::test]
async fn an_escaping_mention_is_refused_in_steer_mode_too() {
    let (_root, working_dir) = workspace();
    let host = Host::boot(&working_dir).await;
    let steering: &dyn TurnInputHandle = &host.steering;

    // No run is active yet, so the queue downgrades a steer request to a
    // queued one; the containment answer must not depend on which.
    let refused = steering
        .submit("actually read @../secrets.txt".into(), TurnInputMode::Steer)
        .expect_err("a steer request is checked like any other");
    assert!(refused.contains("was not sent"), "{refused}");

    host.steering.activate();
    let refused = steering
        .submit("actually read @../secrets.txt".into(), TurnInputMode::Steer)
        .expect_err("and still checked once a run is active");
    assert!(refused.contains("was not sent"), "{refused}");
    assert!(
        host.steering.take_steering().is_empty(),
        "nothing escaping was left for the safe boundary to apply"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The channel path — the backstop, and the non-interactive front end
// ─────────────────────────────────────────────────────────────────────────────

/// The non-interactive front end submits over a `UserIO` channel with no
/// `CommandDispatch` decorator in front of it, so the gate never ran
/// there either. The turn now ends without a model call and says why.
#[tokio::test]
async fn a_headless_prompt_naming_an_escape_never_reaches_the_model() {
    let (_root, working_dir) = workspace();
    let data_dir = tempfile::tempdir().unwrap();
    let working_dir = working_dir.canonicalize().unwrap();

    let (provider, input_tx, _output_rx) = ExecIOProvider::new(64);
    let capturing = CapturingProvider::default();
    let seen = capturing.seen.clone();

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(CapturingModelPlugin {
            provider: capturing,
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(Arc::new(provider)));
    server.finish().await.unwrap();

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    let session_id = SessionId::new();
    let config = AgentConfig::new(MODEL, working_dir);
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            polaris::graph::GraphExecutor::new().with_default_max_iterations(8),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .unwrap();

    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("read @../secrets.txt".to_string()),
        ))
        .await
        .unwrap();
    drop(input_tx);

    let outcome = run::drive_turn(sessions, &session_id).await;

    assert!(
        seen.lock().unwrap().is_empty(),
        "the model was never called: {:?}",
        prompts_seen(&seen)
    );
    let TurnReason::Failed(reason) = &outcome.reason else {
        panic!("a refused prompt ends the turn rather than running it: {outcome:?}");
    };
    assert!(
        reason.contains("outside the working directory"),
        "and the caller is told why: {reason}"
    );
}
