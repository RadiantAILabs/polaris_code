//! Daily-workflow consumer proof: compose, complete, stream, approve, interrupt,
//! and resume, through the **real** host/IO/session path rather than one
//! widget at a time.
//!
//! Every scenario below stands up a [`Server`] wired the way `src/main.rs`
//! wires the interactive front end — the same plugin set, the same
//! [`ConversationPanelPlugin`] decorated by [`terminal::attach_with_status`],
//! the same `/commands` registered by [`SessionSettings`] and
//! [`SessionNavigation`], the same `PolicySnapshot` installed into the session
//! context, and the same turn driver — with exactly two substitutions:
//!
//! - a scripted [`LlmProvider`] stands in for `AnthropicPlugin`, so no key and
//!   no network are involved and every model reply is decided by the test; and
//! - a test-local plugin provides the [`TuiRegistry`] capability without
//!   spawning the terminal runtime, so the real [`ConversationPanel`] the
//!   plugin built can be drained out of the registry and driven with
//!   [`TuiEvent`]s and rendered on a ratatui [`TestBackend`] instead of a
//!   crossterm event loop and an alternate screen.
//!
//! Everything between those two edges is production code: keystrokes go
//! through the panel's composer and popups, submissions cross the bounded
//! channel into [`CommandDispatch`], commands run locally, prompts reach
//! `CodeAgent::receive_user_input`, the answer streams back through
//! `terminal::transcript::forward`, approvals travel as `UserIO::confirm`
//! requests and stamped answers, Ctrl-C reaches `run::request_interrupt`
//! through the panel's host callback, and session switches save, resume, and
//! replay canonical history from the repository-local `FileStore`.
//!
//! Two things from `main.rs` cannot be reached from an integration test and
//! are mirrored here instead, each marked at its definition: the interactive
//! turn-driver loop (session switches happen there, between turns), and the
//! shell configuration constants. Both are copied, not reinterpreted.
//!
//! What this file deliberately does **not** claim: raw-mode setup and
//! restoration, the kitty keyboard negotiation, bracketed-paste enablement
//! and mouse capture all live in the terminal runtime this harness bypasses;
//! they are not covered by these tests. `polaris-tui`'s own
//! `tests/terminal_workflow.rs` covers the presentation side (resize, Unicode
//! caret, browse selection, long transcripts).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolResultContent,
    ToolResultStatus, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{
    AgentTypeId, FileStore, SessionId, SessionStatus, SessionsAPI, SessionsPlugin,
};
use polaris::shell::{ShellConfig, ShellPlugin};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, PermissionMode, PolicySnapshot,
};
use polaris_code::config::ModelCatalog;
use polaris_code::progress::ProgressMiddlewarePlugin;
use polaris_code::run::{self, StatusEmitter};
use polaris_code::terminal::{
    self, CommandRegistry, SessionAction, SessionNavigation, SessionSettings,
};
use polaris_code::tools::{
    CodeGraphPlugin, CodeToolsConfig, CodeToolsPlugin, GraphSearchMode, MemoryToolsPlugin,
};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::{ConversationPanelPlugin, LogBuffer};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::ratatui::buffer::CellWidth as _;
use polaris_tui::{TuiEvent, TuiPanel, TuiRegistry};
use serde_json::json;
use tokio::sync::Notify;

// ─────────────────────────────────────────────────────────────────────────────
// The scripted model
// ─────────────────────────────────────────────────────────────────────────────

/// The model the host boots with, `provider/model` as `SessionSettings`
/// expects it. The provider half must be the scripted provider's name so a
/// `/model` switch onto another `scripted/…` id resolves.
const STARTUP_MODEL: &str = "scripted/startup";
/// The model a `/model` command moves the session onto.
const CHOSEN_MODEL: &str = "scripted/chosen";

/// Billed usage every scripted reply reports, so the meter, the token tally
/// and the cost projection all have something to fold.
const USAGE: Usage = Usage {
    input_tokens: Some(1_000),
    output_tokens: Some(50),
    total_tokens: Some(1_050),
    cache_read_tokens: None,
    cache_creation_tokens: None,
    reasoning_output_tokens: None,
};

/// What one call to the scripted model streams back.
enum Reply {
    /// A plain answer, streamed one word at a time and then closed.
    Text(String),
    /// A plain answer whose second half waits on a gate the test opens, so a
    /// partially streamed message can be observed on screen before it ends.
    Gated {
        /// Streamed immediately.
        head: String,
        /// Streamed once `gate` is notified.
        tail: String,
        /// Opened by the test.
        gate: Arc<Notify>,
    },
    /// One text delta, then a stream that never ends: the shape of a model
    /// call that must be interrupted.
    Hang(String),
    /// A tool call; the arguments are the model's, verbatim.
    Tool {
        /// The provider's tool-call id, which the transcript keys blocks on.
        id: String,
        /// The tool name.
        name: String,
        /// Its arguments.
        args: serde_json::Value,
    },
}

/// The replies queued for the scripted model, consumed one per `stream` call.
/// An empty queue yields a fixed answer, so a turn can never hang for want of
/// a script.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Reply>>,
    calls: AtomicUsize,
}

impl Script {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn next(&self) -> Reply {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Reply::Text("ok".to_string()))
    }

    /// How many times the agent has called the model, across every turn.
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn text_start() -> Result<StreamEvent, GenerationError> {
    Ok(StreamEvent::ContentBlockStart {
        index: 0,
        block: ContentBlockStartData::Text,
    })
}

fn text_delta(text: &str) -> Result<StreamEvent, GenerationError> {
    Ok(StreamEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Text(text.to_string()),
    })
}

fn stop(reason: StopReason) -> Result<StreamEvent, GenerationError> {
    Ok(StreamEvent::MessageStop {
        stop_reason: reason,
        usage: USAGE,
        id: None,
        model: None,
    })
}

/// Split `text` into word-sized deltas, keeping the separating spaces, so a
/// "streamed" answer really does arrive in several pieces.
fn word_deltas(text: &str) -> Vec<Result<StreamEvent, GenerationError>> {
    let mut events = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if ch == ' ' {
            events.push(text_delta(&current));
            current.clear();
        }
    }
    if !current.is_empty() {
        events.push(text_delta(&current));
    }
    events
}

struct ScriptedProvider {
    script: Arc<Script>,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        // Only `recover` generates; the scripted turns never enter it.
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("recovered")],
            usage: USAGE,
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
        let stream: LlmStream = match self.script.next() {
            Reply::Text(text) => {
                let mut events = vec![text_start()];
                events.extend(word_deltas(&text));
                events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                events.push(stop(StopReason::EndTurn));
                futures_util::stream::iter(events).boxed()
            }
            Reply::Gated { head, tail, gate } => {
                let mut before = vec![text_start()];
                before.extend(word_deltas(&head));
                let mut after = word_deltas(&tail);
                after.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                after.push(stop(StopReason::EndTurn));
                futures_util::stream::iter(before)
                    .chain(futures_util::stream::once(async move {
                        gate.notified().await;
                        text_delta("")
                    }))
                    .chain(futures_util::stream::iter(after))
                    .boxed()
            }
            Reply::Hang(text) => futures_util::stream::iter(vec![text_start(), text_delta(&text)])
                .chain(futures_util::stream::pending())
                .boxed(),
            Reply::Tool { id, name, args } => futures_util::stream::iter(vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id,
                        call_id: None,
                        name,
                    },
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: args.to_string(),
                    },
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                stop(StopReason::ToolUse),
            ])
            .boxed(),
        };
        Ok(stream)
    }
}

/// Registers the scripted provider through the seam `AnthropicPlugin` uses.
struct ScriptedModelPlugin {
    script: Arc<Script>,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::terminal_workflow_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the scripted model")
            .register_llm_provider(ScriptedProvider {
                script: self.script.clone(),
            });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The headless terminal
// ─────────────────────────────────────────────────────────────────────────────

/// Provides the [`TuiRegistry`] capability exactly as `TuiPlugin::build` does,
/// and nothing else: no terminal runtime is spawned in `ready`, so the panels
/// contributed to the registry stay there for the test to drain.
struct HeadlessTuiPlugin;

impl Plugin for HeadlessTuiPlugin {
    const ID: &'static str = "test::headless_tui";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The composition root, as main.rs assembles it
// ─────────────────────────────────────────────────────────────────────────────

/// Mirrors `DENIED_COMMANDS` in `src/main.rs`; a binary's `const` is not
/// reachable from an integration test.
const DENIED_COMMANDS: &[&str] = &["rm -rf *", "rm -rf /", "sudo *", ":(){ :|:& };:", "mkfs *"];

/// Mirrors `shell_config` in `src/main.rs` for the attended modes, with one
/// test-only deviation: the executor's cache directory is redirected into the
/// workspace so the proof never creates `~/.polaris`.
fn shell_config(working_dir: &Path) -> ShellConfig {
    ShellConfig::new()
        .with_working_dir(working_dir)
        .with_timeout(120)
        .with_max_output_bytes(64 * 1024)
        .with_allowed_dirs(vec![working_dir.to_path_buf()])
        .with_allowed_commands(vec!["*".to_string()])
        .with_denied_commands(DENIED_COMMANDS.iter().map(|s| (*s).to_string()).collect())
        .with_cache_dir(working_dir.join(".polaris/shell-cache"))
}

/// Mirrors `policy_snapshot` in `src/main.rs` for the default graph-search
/// mode (freeform enabled, so `sparql_query` is not withheld).
fn policy_snapshot(mode: &PermissionMode) -> PolicySnapshot {
    let mut policy = PolicySnapshot::defaults();
    policy.permission = mode.permission_policy();
    policy
}

fn executor() -> polaris::graph::GraphExecutor {
    polaris::graph::GraphExecutor::new().with_default_max_iterations(25)
}

/// The interactive host, assembled the way `main.rs` assembles it.
struct Host {
    server: Server,
    /// The real conversation panel, drained from the registry.
    panel: Box<dyn TuiPanel>,
    /// The scripted model's queue.
    script: Arc<Script>,
    /// The host-owned interrupt entry point, shared with the panel callback.
    interrupt_api: Arc<OnceLock<SessionsAPI>>,
    status_emitter: Arc<OnceLock<StatusEmitter>>,
    output: terminal::TerminalOutput,
    history: Arc<terminal::history::CanonicalHistory>,
    settings: SessionSettings,
    navigation: SessionNavigation,
    config: AgentConfig,
    session_id: SessionId,
    working_dir: PathBuf,
    /// The screen the panel is drawn on.
    terminal: Terminal<TestBackend>,
    /// The driver task, once spawned.
    driver: Option<tokio::task::JoinHandle<()>>,
    /// Test-owned input-boundary gate. Production parks inside `receive`; the
    /// proof waits just outside the turn so assertions can inspect a completed
    /// session without racing the next context lock.
    turn_signal: Arc<Notify>,
    awaiting_turn: Arc<AtomicBool>,
}

impl Host {
    /// Boot the host over `working_dir`, creating session `session_id` — or
    /// resuming it when the repository-local store already has it, exactly
    /// as `main.rs` does for its default session.
    async fn boot(working_dir: &Path, session_id: &str) -> Self {
        let working_dir = working_dir.canonicalize().unwrap();
        let script = Arc::new(Script::default());
        let config = AgentConfig::new(STARTUP_MODEL, working_dir.clone())
            .with_graph_search(GraphSearchMode::default());
        let commands = Arc::new(CommandRegistry::with_builtins());
        let history = Arc::new(terminal::history::CanonicalHistory::default());
        let session_id = SessionId::from_string(session_id);
        let interrupt_api = Arc::new(OnceLock::<SessionsAPI>::new());
        let status_emitter = Arc::new(OnceLock::<StatusEmitter>::new());
        let turn_signal = Arc::new(Notify::new());
        let awaiting_turn = Arc::new(AtomicBool::new(false));

        // One store, as the binary builds it: the session plugin persists
        // through it and `/sessions` reads the saved records back through it.
        let session_store: Arc<dyn polaris::sessions::SessionStore> =
            Arc::new(FileStore::new(working_dir.join(".polaris/sessions")));

        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                script: script.clone(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(&working_dir)))
            .add_plugins(OxigraphPlugin::in_memory().with_tools(true))
            .add_plugins(CodeGraphPlugin::new(&working_dir))
            .add_plugins(MemoryToolsPlugin::new(&working_dir))
            .add_plugins(ShellPlugin::new(shell_config(&working_dir)))
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(session_store.clone()))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ProgressMiddlewarePlugin)
            .add_plugins(HeadlessTuiPlugin);

        let log_buffer = LogBuffer::with_capacity(2_000);
        let callback_api = interrupt_api.clone();
        let callback_id = session_id.clone();
        let (conversation, output) = terminal::attach_with_status(
            ConversationPanelPlugin::default()
                .with_log_buffer(log_buffer)
                .with_history_source(history.clone())
                .with_interrupt_handle(move || {
                    let Some(sessions) = callback_api.get().cloned() else {
                        return;
                    };
                    run::request_interrupt(&sessions, &callback_id);
                }),
            commands.clone(),
            &working_dir,
            status_emitter.clone(),
        );
        server.add_plugins(conversation);
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let panel = server
            .api::<TuiRegistry>()
            .unwrap()
            .drain()
            .into_iter()
            .find(|panel| panel.id() == "conversation")
            .expect("the conversation panel plugin contributed its panel");

        let settings = {
            let models = server.get_global::<ModelRegistry>().unwrap();
            let catalog = ModelCatalog::new(config.model_id.clone(), [STARTUP_MODEL, CHOSEN_MODEL])
                .restricted_to_providers(&models.llm_provider_names());
            SessionSettings::new(config.model_id.clone(), PermissionMode::Confirm, &catalog)
        };
        settings.register(&commands).unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        let _ = interrupt_api.set(sessions.clone());
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let agent_type = AgentTypeId::from_name(CodeAgent::NAME);
        let resumed = sessions
            .resume_session_with_executor(server.create_context(), &session_id, executor(), {
                let cfg = config.clone();
                let settings = settings.clone();
                move |ctx| {
                    ctx.insert(cfg.clone());
                    ctx.insert(settings.clone());
                }
            })
            .await
            .is_ok();
        if !resumed {
            sessions
                .create_session_with_executor(
                    server.create_context(),
                    &session_id,
                    &agent_type,
                    executor(),
                    {
                        let cfg = config.clone();
                        let settings = settings.clone();
                        move |ctx| {
                            ctx.insert(cfg.clone());
                            ctx.insert(settings.clone());
                        }
                    },
                )
                .unwrap();
        }

        let navigation = SessionNavigation::new(sessions.clone(), session_id.clone())
            .with_store(session_store.clone());
        navigation.register(&commands).unwrap();

        let policy = policy_snapshot(&PermissionMode::Confirm);
        sessions
            .with_context(&session_id, move |ctx| {
                ctx.insert(policy);
            })
            .await
            .unwrap();
        history.refresh(sessions, &session_id).await.unwrap();

        Self {
            server,
            panel,
            script,
            interrupt_api,
            status_emitter,
            output,
            history,
            settings,
            navigation,
            config,
            session_id,
            working_dir,
            terminal: Terminal::new(TestBackend::new(160, 30)).unwrap(),
            driver: None,
            turn_signal,
            awaiting_turn,
        }
    }

    fn sessions(&self) -> &SessionsAPI {
        self.server.api::<SessionsAPI>().unwrap()
    }

    /// Spawn the interactive turn driver.
    ///
    /// This is a mirror of the `tokio::spawn` block in `src/main.rs`: one turn
    /// per user submission, canonical history refreshed after every turn, and
    /// a pending `/new` or `/resume` performed between turns — save the old
    /// session, create or resume the target, reset settings and permission
    /// mode to the fail-closed defaults, refresh and replay canonical history.
    /// The durability point after each turn and the error policy are
    /// `run::persist_turn`'s and `run::DriverStep`'s, unchanged.
    fn spawn_driver(&mut self) {
        let driver_sessions = self.sessions().clone();
        let mut driver_id = self.navigation.current();
        let driver_status = self.status_emitter.clone();
        let driver_history = self.history.clone();
        let driver_navigation = self.navigation.clone();
        let driver_output = self.output.clone();
        let driver_config = self.config.clone();
        let driver_settings = self.settings.clone();
        let driver_agent_type = AgentTypeId::from_name(CodeAgent::NAME);
        let turn_signal = self.turn_signal.clone();
        let awaiting_turn = self.awaiting_turn.clone();
        self.driver = Some(tokio::spawn(async move {
            let mut budget = run::FailureBudget::new();
            let mut first_turn = true;
            loop {
                if first_turn {
                    first_turn = false;
                } else {
                    let _ = driver_output
                        .send(
                            IOMessage::from_agent("code", IOContent::Text(String::new()))
                                .with_metadata(IO_TYPE, "status")
                                .with_metadata("node", "receive_user_input")
                                .with_metadata("seq", "0"),
                        )
                        .await;
                    awaiting_turn.store(true, Ordering::SeqCst);
                    turn_signal.notified().await;
                    awaiting_turn.store(false, Ordering::SeqCst);
                }
                let (_outcome, step) =
                    run::drive_turn_step(&driver_sessions, &driver_id, driver_status.get()).await;
                let _ = driver_history.refresh(&driver_sessions, &driver_id).await;
                if let Err(message) = run::persist_turn(&driver_sessions, &driver_id).await {
                    let _ = driver_output
                        .send(IOMessage::system_text(message).with_metadata(IO_TYPE, "error"))
                        .await;
                }
                if let Some(action) = driver_navigation.take_pending() {
                    let target = match &action {
                        SessionAction::New(id) | SessionAction::Resume(id) => id.clone(),
                    };
                    if let Err(err) = driver_sessions.save_session(&driver_id).await {
                        let _ = driver_output
                            .send(
                                IOMessage::system_text(format!(
                                    "Session switch cancelled: failed to save `{}`: {err}. Fix storage and retry.",
                                    driver_id.as_str()
                                ))
                                .with_metadata(IO_TYPE, "error"),
                            )
                            .await;
                        continue;
                    }
                    let seed = {
                        let cfg = driver_config.clone();
                        let settings = driver_settings.clone();
                        move |ctx: &mut polaris::system::param::SystemContext<'static>| {
                            ctx.insert(cfg.clone());
                            ctx.insert(settings.clone());
                        }
                    };
                    let transition = match action {
                        SessionAction::Resume(_) => {
                            driver_sessions
                                .resume_session_with_executor(
                                    driver_sessions.create_context(),
                                    &target,
                                    executor(),
                                    seed,
                                )
                                .await
                        }
                        SessionAction::New(_) => driver_sessions.create_session_with_executor(
                            driver_sessions.create_context(),
                            &target,
                            &driver_agent_type,
                            executor(),
                            seed,
                        ),
                    };
                    if let Err(err) = transition {
                        let _ = driver_output
                            .send(
                                IOMessage::system_text(format!(
                                    "Session switch failed for `{}`: {err}. The previous session remains active; fix the id or storage and retry.",
                                    target.as_str()
                                ))
                                .with_metadata(IO_TYPE, "error"),
                            )
                            .await;
                        continue;
                    }
                    let fresh_policy = policy_snapshot(&PermissionMode::Confirm);
                    driver_settings.reset(driver_config.model_id.clone());
                    if driver_sessions
                        .with_context(&target, move |ctx| {
                            ctx.insert(fresh_policy);
                        })
                        .await
                        .is_err()
                    {
                        continue;
                    }
                    if driver_history
                        .refresh(&driver_sessions, &target)
                        .await
                        .is_err()
                    {
                        continue;
                    }
                    let _ = driver_output
                        .send(polaris_tui::commands::session_replay_start(target.as_str()))
                        .await;
                    if let Ok(messages) = driver_history.replay() {
                        for message in messages {
                            let _ = driver_output.send(message).await;
                        }
                    }
                    let _ = driver_output
                        .send(polaris_tui::commands::session_replay_end(target.as_str()))
                        .await;
                    driver_id = target.clone();
                    driver_navigation.set_current(target);
                    continue;
                }
                let step = budget.absorb(step);
                if let Some(message) = step.message() {
                    let _ = driver_output
                        .send(IOMessage::system_text(message).with_metadata(IO_TYPE, "error"))
                        .await;
                }
                if !step.continues() {
                    return;
                }
            }
        }));
    }

    // — Driving the terminal ————————————————————————————————————————

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.panel
            .handle_event(&TuiEvent::Key(KeyEvent::new(code, modifiers)));
    }

    /// Type `text` one keystroke at a time, as a user would.
    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    fn enter(&mut self) {
        if self.awaiting_turn.swap(false, Ordering::SeqCst) {
            self.turn_signal.notify_one();
        }
        self.key(KeyCode::Enter, KeyModifiers::NONE);
    }

    /// Type a line and submit it.
    async fn submit(&mut self, text: &str) {
        self.type_text(text);
        // Completion sources are asynchronous. Give the panel one runtime
        // cycle to install a `/` or `@` popup before Enter decides whether to
        // accept a completion or submit the draft.
        let _ = self.frame().await;
        self.enter();
        // Ensure the accepted/queued state is observable before callers wait
        // for the host to become ready again.
        let _ = self.frame().await;
    }

    fn paste(&mut self, text: &str) {
        self.panel.handle_event(&TuiEvent::Paste(text.to_string()));
    }

    fn ctrl_c(&mut self) {
        self.key(KeyCode::Char('c'), KeyModifiers::CONTROL);
    }

    /// Stop the test-owned driver at an input boundary so assertions can read
    /// the session context without racing the next parked receive.
    async fn pause_driver(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
            let _ = driver.await;
            tokio::task::yield_now().await;
        }
    }

    /// Resize the screen the panel is drawn on and tell the panel.
    fn resize(&mut self, width: u16, height: u16) {
        self.terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        self.panel.handle_event(&TuiEvent::Resize {
            cols: width,
            rows: height,
        });
    }

    /// Let the runtime run, tick the panel, and draw one frame.
    ///
    /// The scripted agent runs on this same runtime, so yielding advances it;
    /// the short sleep lets blocking-pool work (indexing, the shell, the
    /// session store) finish between frames.
    async fn frame(&mut self) -> Vec<String> {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.panel.tick();
        let panel = &mut self.panel;
        self.terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        screen_rows(self.terminal.backend())
    }

    /// Redraw until `predicate` holds, or fail with the last screen.
    async fn wait_for(&mut self, what: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self.frame().await.join("\n");
            if predicate(&screen) {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; last screen:\n{screen}"
            );
        }
    }

    /// Wait for a completed turn: the host reports idle and the panel's meta
    /// row reads ready again.
    async fn wait_ready(&mut self) -> String {
        self.wait_for("the ready meta row", |screen| screen.contains("● ready"))
            .await
    }

    async fn wait_session(&mut self, id: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let screen = self.frame().await.join("\n");
            if self.navigation.current().as_str() == id {
                return screen;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for session `{id}`; current `{}`; last screen:\n{screen}",
                self.navigation.current().as_str()
            );
        }
    }

    // — Reading back the session ————————————————————————————————————

    /// The canonical conversation of `session`, read from the live context.
    async fn canonical(&self, session: &SessionId) -> Vec<Message> {
        self.sessions()
            .with_context(session, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .unwrap()
                    .messages
                    .clone()
            })
            .await
            .unwrap()
    }

    /// Every user prompt text in the canonical history, in order.
    async fn user_prompts(&self, session: &SessionId) -> Vec<String> {
        self.canonical(session)
            .await
            .into_iter()
            .filter_map(|message| match message {
                Message::User { content } => content.iter().find_map(|block| match block {
                    UserBlock::Text(text) => Some(text.text.clone()),
                    _ => None,
                }),
                Message::Assistant { .. } => None,
            })
            .collect()
    }

    /// Tool-call ids the assistant issued and tool-result ids the user side
    /// answered with, so provider validity (every call paired) can be asserted.
    async fn tool_pairs(&self, session: &SessionId) -> (Vec<String>, Vec<(String, bool)>) {
        let mut uses = Vec::new();
        let mut results = Vec::new();
        for message in self.canonical(session).await {
            match message {
                Message::Assistant { content, .. } => {
                    for block in content {
                        if let AssistantBlock::ToolCall(call) = block {
                            uses.push(call.id.clone());
                        }
                    }
                }
                Message::User { content } => {
                    for block in content {
                        if let UserBlock::ToolResult(result) = block {
                            assert!(matches!(result.content, ToolResultContent::Text(_)));
                            results.push((
                                result.id.clone(),
                                result.status == ToolResultStatus::Error,
                            ));
                        }
                    }
                }
            }
        }
        (uses, results)
    }

    fn workspace_file(&self, name: &str) -> String {
        std::fs::read_to_string(self.working_dir.join(name)).unwrap_or_default()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

/// The rows a terminal would show, with the cells hidden behind a wide glyph
/// skipped so `你好` reads as `你好` and not `你 好 `.
fn screen_rows(backend: &TestBackend) -> Vec<String> {
    let buffer = backend.buffer();
    buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|cells| {
            let mut row = String::new();
            let mut skip: u16 = 0;
            for cell in cells {
                if skip == 0 {
                    row.push_str(cell.symbol());
                }
                skip = skip.max(cell.cell_width()).saturating_sub(1);
            }
            row.trim_end().to_string()
        })
        .collect()
}

/// A disposable workspace with a couple of files the daily workflow can
/// mention, read and edit.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn answer() -> u32 {\n    42\n}\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("README.md"), "# fixture\n").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "old line\n").unwrap();
    dir
}

fn edit_reply(id: &str, path: &str, old: &str, new: &str) -> Reply {
    Reply::Tool {
        id: id.to_string(),
        name: "edit_file".to_string(),
        args: json!({ "path": path, "old_string": old, "new_string": new, "replace_all": false }),
    }
}

fn command_reply(id: &str, command: &str) -> Reply {
    Reply::Tool {
        id: id.to_string(),
        name: "run_command".to_string(),
        args: json!({ "command": command }),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Compose, stream, status
// ─────────────────────────────────────────────────────────────────────────────

/// A typed prompt reaches the agent, the answer arrives on screen *while* it
/// is still streaming, and the meta row reports the model, the projected
/// context and the session the turn ran in.
#[tokio::test]
async fn a_typed_prompt_streams_its_answer_onto_the_screen_before_the_turn_ends() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    let gate = Arc::new(Notify::new());
    host.script.push(Reply::Gated {
        head: "The answer is ".to_string(),
        tail: "forty-two.".to_string(),
        gate: gate.clone(),
    });
    host.spawn_driver();

    host.submit("what is the answer?").await;
    // The local echo carries the queued marker until the agent takes it.
    let screen = host
        .wait_for("the echoed prompt", |screen| {
            screen.contains("you ❯ what is the answer?")
        })
        .await;
    assert!(screen.contains("what is the answer?"), "{screen}");

    // Mid-stream: the head is on screen, the tail is not, and the host is busy.
    let screen = host
        .wait_for("the streamed head", |screen| {
            screen.contains("The answer is")
        })
        .await;
    assert!(
        !screen.contains("forty-two"),
        "the tail must not be visible before the gate opens:\n{screen}"
    );
    assert!(
        screen.contains("Thinking…"),
        "the progress bridge must report the model call while it streams:\n{screen}"
    );
    assert!(
        !screen.contains("· queued"),
        "a prompt the agent has taken must lose its queued marker:\n{screen}"
    );

    gate.notify_one();
    let screen = host.wait_ready().await;
    assert!(
        screen.contains("code ❯ The answer is forty-two."),
        "the closed block carries the full answer once:\n{screen}"
    );
    assert_eq!(screen.matches("forty-two").count(), 1, "{screen}");
    // Host-owned facts on the meta row: model, estimated context, session/turn,
    // and the token tally the stream billed.
    assert!(screen.contains(STARTUP_MODEL), "{screen}");
    assert!(screen.contains("ctx ~"), "{screen}");
    assert!(screen.contains("default · turn"), "{screen}");
    assert!(screen.contains("↑1.0k ↓50"), "{screen}");

    let session = host.session_id.clone();
    host.pause_driver().await;
    assert_eq!(
        host.user_prompts(&session).await,
        vec!["what is the answer?".to_string()]
    );
    assert_eq!(
        host.sessions().session_info(&session).unwrap().turn_number,
        1
    );
}

/// A bracketed paste with newlines is one prompt, not one per line, and a
/// wide-character prompt survives the composer, the channel and canonical
/// history byte for byte.
#[tokio::test]
async fn pasted_and_wide_prompts_reach_history_intact() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.script.push(Reply::Text("noted.".to_string()));
    host.script.push(Reply::Text("你好!".to_string()));
    host.spawn_driver();

    host.paste("first line\nsecond line\n/model not-a-command");
    host.enter();
    host.wait_ready().await;

    let wide = "你好 🌍 café — naïve ünïcödé";
    host.submit(wide).await;
    let screen = host
        .wait_for("the wide answer", |screen| screen.contains("code ❯ 你好!"))
        .await;
    assert!(screen.contains("you ❯ 你好 🌍 café"), "{screen}");

    let session = host.session_id.clone();
    host.pause_driver().await;
    assert_eq!(
        host.user_prompts(&session).await,
        vec![
            "first line\nsecond line\n/model not-a-command".to_string(),
            wide.to_string()
        ],
        "a paste is one draft, its newlines never submit, and a pasted `/` never dispatches"
    );
    assert_eq!(
        host.settings.snapshot().model_id,
        STARTUP_MODEL,
        "the pasted /model line must not have run as a command"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Commands and file completion
// ─────────────────────────────────────────────────────────────────────────────

/// Slash commands run locally without a model call, the `/` and `@` popups
/// complete from the host's real registry and workspace, an accepted mention
/// is stamped onto the forwarded prompt, and an escaping mention is refused.
#[tokio::test]
async fn commands_and_file_mentions_complete_from_the_real_host() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();

    // The command set is advertised on the agent's first receive; the popup
    // then lists what the host actually registered.
    host.type_text("/");
    let screen = host
        .wait_for("the command popup", |screen| {
            ["help", "model", "new", "permission", "resume", "sessions"]
                .iter()
                .all(|name| screen.contains(name))
        })
        .await;
    assert!(screen.contains("List the available commands"), "{screen}");

    // `/he` + Enter fills the highlighted match and runs it.
    host.type_text("he");
    host.enter();
    let _ = host.frame().await;
    host.enter();
    let screen = host
        .wait_for("the help reply", |screen| screen.contains("/help ❯"))
        .await;
    assert!(
        screen.contains("/sessions"),
        "help lists the session commands:\n{screen}"
    );
    assert_eq!(
        host.script.calls(),
        0,
        "a local command never calls the model"
    );

    // An unknown command is answered, not forwarded.
    host.submit("/nosuch").await;
    host.wait_for("the unknown-command error", |screen| {
        screen.contains("error ❯") && screen.contains("/help")
    })
    .await;
    assert_eq!(host.script.calls(), 0);

    // `@RE` completes README.md from the workspace walk; Tab accepts it.
    host.type_text("@RE");
    host.wait_for("the file popup", |screen| screen.contains("README.md"))
        .await;
    host.key(KeyCode::Tab, KeyModifiers::NONE);
    host.script
        .push(Reply::Text("It is a fixture.".to_string()));
    host.submit("explain").await;
    host.wait_ready().await;

    let session = host.session_id.clone();
    assert_eq!(
        host.user_prompts(&session).await,
        vec!["@README.md explain".to_string()]
    );
    assert_eq!(host.script.calls(), 1);

    // A mention resolving outside the working directory is refused with an
    // error and never reaches the model.
    let outside = workspace.path().join("../escape-target");
    std::fs::write(&outside, "secret").unwrap();
    host.submit("@../escape-target read this").await;
    let screen = host
        .wait_for("the mention refusal", |screen| {
            screen.contains("was not sent") && screen.contains("outside the working directory")
        })
        .await;
    assert!(screen.contains("error ❯"), "{screen}");
    let _ = std::fs::remove_file(outside);
    assert_eq!(
        host.script.calls(),
        1,
        "a refused prompt never reaches the model"
    );
    host.pause_driver().await;
    assert_eq!(host.user_prompts(&session).await.len(), 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Approvals
// ─────────────────────────────────────────────────────────────────────────────

/// An `edit_file` approval shows the unified diff, `y` runs exactly that edit,
/// `n` and `Esc` deny a command and leave the filesystem untouched, and every
/// tool call stays paired in canonical history.
#[tokio::test]
async fn edits_and_commands_are_approved_or_denied_through_the_dialog() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();

    // Turn 1: approve an edit.
    host.script
        .push(edit_reply("call-edit", "notes.txt", "old line", "new line"));
    host.script.push(Reply::Text("Edited.".to_string()));
    host.submit("replace old with new").await;
    let screen = host
        .wait_for("the edit approval", |screen| screen.contains("approve?"))
        .await;
    assert!(screen.contains("approve ❯ Edit 'notes.txt':"), "{screen}");
    assert!(
        screen.contains("--- a/notes.txt"),
        "a structured diff, not ANSI:\n{screen}"
    );
    assert!(screen.contains("+new line"), "{screen}");
    assert!(
        screen.contains("… edit_file"),
        "the tool block waits as pending:\n{screen}"
    );
    assert_eq!(
        host.workspace_file("notes.txt"),
        "old line\n",
        "nothing runs before the answer"
    );

    host.submit("y").await;
    let screen = host.wait_ready().await;
    assert!(screen.contains("✓ edit_file"), "{screen}");
    assert!(screen.contains("code ❯ Edited."), "{screen}");
    assert!(
        !screen.contains("approve?"),
        "the banner closes with the dialog:\n{screen}"
    );
    assert_eq!(host.workspace_file("notes.txt"), "new line\n");

    // Turn 2: deny a command by typing `n`.
    let marker = host.working_dir.join("ran.log");
    let command = format!("printf 'ran\\n' >> '{}'", marker.display());
    host.script.push(command_reply("call-run-1", &command));
    host.script
        .push(Reply::Text("Denied, understood.".to_string()));
    host.submit("run the marker").await;
    let screen = host
        .wait_for("the command approval", |screen| screen.contains("approve?"))
        .await;
    assert!(screen.contains("approve ❯ Run command:"), "{screen}");
    host.submit("n").await;
    let screen = host.wait_ready().await;
    assert!(screen.contains("⊘ run_command"), "{screen}");
    assert!(screen.contains("User denied execution"), "{screen}");
    assert!(!marker.exists(), "a denied command must not run");

    // Turn 3: deny with Esc.
    host.script.push(command_reply("call-run-2", &command));
    host.script.push(Reply::Text("Still denied.".to_string()));
    host.submit("try again").await;
    host.wait_for("the second approval", |screen| screen.contains("approve?"))
        .await;
    host.key(KeyCode::Esc, KeyModifiers::NONE);
    host.wait_ready().await;
    assert!(!marker.exists(), "Esc denies too");

    // Turn 4: approve the command; it runs exactly once.
    host.script.push(command_reply("call-run-3", &command));
    host.script.push(Reply::Text("Ran.".to_string()));
    host.submit("really run it").await;
    host.wait_for("the third approval", |screen| screen.contains("approve?"))
        .await;
    host.submit("yes").await;
    let screen = host.wait_ready().await;
    assert!(screen.contains("✓ run_command"), "{screen}");
    assert_eq!(
        host.workspace_file("ran.log"),
        "ran\n",
        "exactly one process ran"
    );

    // Every tool call the model issued has exactly one paired result, with
    // the denials recorded as errors — provider-valid history.
    let session = host.session_id.clone();
    host.pause_driver().await;
    let (uses, results) = host.tool_pairs(&session).await;
    assert_eq!(
        uses,
        ["call-edit", "call-run-1", "call-run-2", "call-run-3"]
    );
    assert_eq!(
        results,
        [
            ("call-edit".to_string(), false),
            ("call-run-1".to_string(), true),
            ("call-run-2".to_string(), true),
            ("call-run-3".to_string(), false),
        ]
    );
    assert_eq!(
        host.sessions().session_info(&session).unwrap().turn_number,
        4
    );
}

/// A prompt typed while an approval is open is neither the answer nor lost:
/// it waits behind the dialog and becomes the next turn.
#[tokio::test]
async fn a_prompt_typed_during_an_approval_becomes_the_next_turn() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();

    host.script.push(edit_reply(
        "call-edit",
        "notes.txt",
        "old line",
        "newer line",
    ));
    host.script.push(Reply::Text("Applied.".to_string()));
    host.script
        .push(Reply::Text("Second turn answer.".to_string()));
    host.submit("edit it").await;
    host.wait_for("the approval", |screen| screen.contains("approve?"))
        .await;

    host.submit("and then tell me a joke").await;
    let screen = host
        .wait_for("the queued notice", |screen| {
            screen.contains("queued for the next turn")
        })
        .await;
    assert!(
        screen.contains("approve?"),
        "the dialog is still open:\n{screen}"
    );
    assert_eq!(host.workspace_file("notes.txt"), "old line\n");

    host.submit("y").await;
    host.wait_for("the first turn", |screen| {
        screen.contains("code ❯ Applied.")
    })
    .await;
    host.turn_signal.notify_one();
    let screen = host
        .wait_for("both turns", |screen| {
            screen.contains("Second turn answer.")
        })
        .await;
    assert!(screen.contains("✓ edit_file"), "{screen}");
    assert_eq!(host.workspace_file("notes.txt"), "newer line\n");

    let session = host.session_id.clone();
    host.pause_driver().await;
    assert_eq!(
        host.user_prompts(&session).await,
        vec!["edit it".to_string(), "and then tell me a joke".to_string()]
    );
    assert_eq!(
        host.sessions().session_info(&session).unwrap().turn_number,
        2
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Interrupt, then continue
// ─────────────────────────────────────────────────────────────────────────────

/// Ctrl-C during a streaming model call reaches the session's cancellation
/// signal through the panel's host callback; the host acknowledges with
/// `Interrupting` then `Cancelled`, keeps the partial text, leaves the session
/// usable, and the next prompt runs a complete turn in the same session.
#[tokio::test]
async fn ctrl_c_interrupts_a_streaming_turn_and_the_session_continues() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.script.push(Reply::Hang("Working on".to_string()));
    host.spawn_driver();

    host.submit("do something slow").await;
    let screen = host
        .wait_for("the hanging stream", |screen| {
            screen.contains("Working on") && screen.contains("Thinking…")
        })
        .await;
    assert!(!screen.contains("Interrupting"), "{screen}");

    host.ctrl_c();
    let screen = host
        .wait_for("the cancellation rows", |screen| {
            screen.contains("Cancelled ❯")
        })
        .await;
    let interrupting = screen
        .find("Interrupting ❯")
        .expect("the host acknowledged the signal");
    let cancelled = screen.find("Cancelled ❯").unwrap();
    assert!(interrupting < cancelled, "in order:\n{screen}");
    assert!(
        screen.contains("Working on"),
        "the partial text survives the interrupt:\n{screen}"
    );
    assert!(screen.contains("response ended early"), "{screen}");
    let screen = host.wait_ready().await;
    assert!(!screen.contains("Thinking…"), "{screen}");

    let session = host.session_id.clone();
    assert_eq!(
        host.sessions().session_info(&session).unwrap().status,
        SessionStatus::Active
    );
    assert!(
        host.sessions()
            .checkpoints()
            .list(&session)
            .unwrap()
            .iter()
            .any(|point| point.resume_turn == 1)
    );

    host.script.push(Reply::Text("Back to work.".to_string()));
    host.submit("continue").await;
    let screen = host
        .wait_for("the resumed turn", |screen| {
            screen.contains("code ❯ Back to work.")
        })
        .await;
    assert!(screen.contains("you ❯ continue"), "{screen}");
    host.wait_ready().await;
    host.pause_driver().await;
    assert_eq!(
        host.user_prompts(&session).await,
        vec!["do something slow".to_string(), "continue".to_string()]
    );
    assert_eq!(
        host.sessions().session_info(&session).unwrap().turn_number,
        2
    );
}

/// Ctrl-C on an idle panel with nothing to clear is declined so the shell can
/// quit; it does not raise the session signal.
#[tokio::test]
async fn ctrl_c_while_idle_falls_through_to_the_shell() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();
    host.script.push(Reply::Text("hi".to_string()));
    host.submit("hello").await;
    host.wait_ready().await;

    let outcome = host.panel.handle_event(&TuiEvent::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert_eq!(outcome, polaris_tui::EventOutcome::Continue);
    let screen = host.frame().await.join("\n");
    assert!(!screen.contains("Interrupting"), "{screen}");
    assert!(
        host.interrupt_api.get().is_some(),
        "the callback target was installed at boot"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Sessions and settings
// ─────────────────────────────────────────────────────────────────────────────

/// `/sessions`, `/new` and `/resume` save, switch and replay through the
/// repository-local store; the replayed transcript is the canonical history;
/// and a fresh host over the same workspace resumes what was saved.
#[tokio::test]
async fn sessions_are_saved_switched_replayed_and_resumed_after_a_restart() {
    let workspace = workspace();
    let first_session = SessionId::from_string("default");
    {
        let mut host = Host::boot(workspace.path(), "default").await;
        host.spawn_driver();

        host.script.push(Reply::Text("first answer".to_string()));
        host.submit("first prompt").await;
        host.wait_for("the first answer", |screen| {
            screen.contains("code ❯ first answer")
        })
        .await;
        host.wait_ready().await;

        host.submit("/sessions").await;
        host.wait_for("the session list", |screen| {
            screen.contains("/sessions ❯")
                && screen.contains("Sessions for this repository (current default)")
        })
        .await;

        // `/new second`: saved, switched, replayed (an empty conversation).
        host.submit("/new second").await;
        let screen = host.wait_session("second").await;
        assert!(
            !screen.contains("first answer"),
            "a switch clears the previous session's transcript:\n{screen}"
        );
        assert!(
            workspace.path().join(".polaris/sessions").is_dir(),
            "the store lives inside the repository"
        );
        assert!(
            host.sessions()
                .list_sessions()
                .await
                .unwrap()
                .contains(&first_session),
            "the previous session was saved before the switch"
        );

        host.script.push(Reply::Text("second answer".to_string()));
        host.submit("second prompt").await;
        host.wait_for("the second answer", |screen| {
            screen.contains("code ❯ second answer")
        })
        .await;
        host.wait_ready().await;
        let second = SessionId::from_string("second");
        host.pause_driver().await;
        assert_eq!(
            host.user_prompts(&second).await,
            vec!["second prompt".to_string()],
            "the new session starts empty"
        );
        host.spawn_driver();

        // Now that a session is saved, `/sessions` is a picker: it numbers
        // what the store holds and says enough about each row to recognise it
        // without knowing the identifier.
        host.submit("/sessions").await;
        let listed = host
            .wait_for("the numbered picker", |screen| {
                screen.contains("default") && screen.contains("second")
            })
            .await;
        assert!(
            listed.contains("first prompt"),
            "a row says what the session was opened to do:\n{listed}"
        );
        assert!(
            listed.contains("/resume <number>"),
            "the picker says how to choose from it:\n{listed}"
        );

        // Choosing default's row replays the first conversation from canonical
        // history without running anything — the same as typing the identifier
        // did. The picker is newest-first, so a real gap between creating the
        // two sessions may put `second` before `default`.
        let default_row = if listed.contains("1. default") {
            1
        } else {
            assert!(listed.contains("2. default"), "{listed}");
            2
        };
        let turns_before = host
            .sessions()
            .session_info(&first_session)
            .unwrap()
            .turn_number;
        host.submit(&format!("/resume {default_row}")).await;
        let screen = host.wait_session("default").await;
        assert!(screen.contains("you ❯ first prompt"), "{screen}");
        assert!(!screen.contains("second answer"), "{screen}");
        assert_eq!(
            host.sessions()
                .session_info(&first_session)
                .unwrap()
                .turn_number,
            turns_before,
            "replay runs no turn"
        );

        // A switch to the active session is refused; a bogus resume fails
        // visibly and leaves the current session active.
        host.submit("/resume default").await;
        host.wait_for("the already-active error", |screen| {
            screen.contains("already active")
        })
        .await;
        host.submit("/resume nonexistent").await;
        host.wait_for("the failed switch", |screen| {
            screen.contains("Session switch failed for `nonexistent`")
        })
        .await;

        host.script.push(Reply::Text("third answer".to_string()));
        host.submit("third prompt").await;
        host.wait_for("the third answer", |screen| {
            screen.contains("code ❯ third answer")
        })
        .await;
        host.wait_ready().await;
        host.pause_driver().await;
        assert_eq!(
            host.user_prompts(&first_session).await,
            vec!["first prompt".to_string(), "third prompt".to_string()]
        );
        // Persist the way the binary does on exit.
        host.sessions().save_session(&first_session).await.unwrap();
    }

    // Restart: a new host over the same workspace resumes the saved session
    // and its canonical history replays intact.
    let mut host = Host::boot(workspace.path(), "default").await;
    assert_eq!(
        host.user_prompts(&first_session).await,
        vec!["first prompt".to_string(), "third prompt".to_string()],
        "resume restores the canonical conversation from the file store"
    );
    let replay = host.history.replay().unwrap();
    let bodies: Vec<String> = replay
        .iter()
        .filter_map(|message| match &message.content {
            IOContent::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        bodies,
        [
            "first prompt",
            "first answer",
            "third prompt",
            "third answer"
        ]
    );
    host.spawn_driver();
    host.script.push(Reply::Text("fourth answer".to_string()));
    host.submit("fourth prompt").await;
    host.wait_for("the post-restart answer", |screen| {
        screen.contains("code ❯ fourth answer")
    })
    .await;
    host.wait_ready().await;
    host.pause_driver().await;
    assert_eq!(
        host.sessions()
            .session_info(&first_session)
            .unwrap()
            .turn_number,
        3
    );
}

/// `/model` and `/permission` change the next turn: the meta row names the
/// chosen model, plan mode denies an edit without asking, and a session
/// entered through `/new` starts back in fail-closed confirm mode.
#[tokio::test]
async fn interactive_settings_apply_at_the_next_turn_boundary() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();

    host.submit("/model").await;
    host.wait_for("the model reply", |screen| {
        screen.contains("/model ❯") && screen.contains(STARTUP_MODEL)
    })
    .await;
    host.submit(&format!("/model {CHOSEN_MODEL}")).await;
    host.wait_for("the model change", |screen| {
        screen.contains(&format!("Model set to {CHOSEN_MODEL} for the next turn"))
    })
    .await;
    host.submit("/model nope/unknown").await;
    host.wait_for("the refused provider", |screen| {
        screen.contains("is not configured")
    })
    .await;
    assert_eq!(host.settings.snapshot().model_id, CHOSEN_MODEL);

    host.script
        .push(Reply::Text("on the chosen model".to_string()));
    host.submit("which model?").await;
    let screen = host.wait_ready().await;
    assert!(
        screen.contains(CHOSEN_MODEL),
        "the meta row must name the model the turn ran on:\n{screen}"
    );

    // Plan mode: the edit is denied by policy, so no dialog opens.
    host.submit("/permission plan").await;
    host.wait_for("the permission change", |screen| {
        screen.contains("Permission mode set to plan for the next turn")
    })
    .await;
    host.script.push(edit_reply(
        "call-plan-edit",
        "notes.txt",
        "old line",
        "planned",
    ));
    host.script
        .push(Reply::Text("I cannot edit in plan mode.".to_string()));
    host.submit("edit it").await;
    let screen = host.wait_ready().await;
    assert!(screen.contains("⊘ edit_file"), "{screen}");
    assert!(screen.contains("Permission denied"), "{screen}");
    assert!(!screen.contains("approve?"), "{screen}");
    assert_eq!(host.workspace_file("notes.txt"), "old line\n");

    host.submit("/permission").await;
    host.wait_for("the mode reply", |screen| {
        screen.contains("Active permission mode: plan")
    })
    .await;

    // A session entered through /new starts in confirm mode on the startup
    // model regardless of the settings the previous session had.
    host.submit("/new fresh").await;
    host.wait_session("fresh").await;
    assert_eq!(
        host.settings.snapshot().permission_mode,
        PermissionMode::Confirm
    );
    assert_eq!(host.settings.snapshot().model_id, STARTUP_MODEL);
    host.script.push(edit_reply(
        "call-fresh-edit",
        "notes.txt",
        "old line",
        "fresh line",
    ));
    host.script.push(Reply::Text("Done.".to_string()));
    host.submit("edit it here").await;
    let screen = host
        .wait_for("the confirm dialog in the fresh session", |screen| {
            screen.contains("approve?")
        })
        .await;
    assert!(screen.contains(STARTUP_MODEL), "{screen}");
    host.submit("y").await;
    host.wait_ready().await;
    assert_eq!(host.workspace_file("notes.txt"), "fresh line\n");
}

// ─────────────────────────────────────────────────────────────────────────────
// Narrow terminals
// ─────────────────────────────────────────────────────────────────────────────

/// A real turn on a narrow screen: the panel keeps rendering, the meta row
/// drops its facts before it drops the activity, and the transcript is still
/// readable after a resize back up.
#[tokio::test]
async fn a_narrow_terminal_keeps_the_workflow_readable() {
    let workspace = workspace();
    let mut host = Host::boot(workspace.path(), "default").await;
    host.spawn_driver();
    host.resize(36, 10);

    host.script.push(Reply::Text(
        "a fairly long answer that has to wrap on a narrow terminal".to_string(),
    ));
    host.submit("narrow").await;
    let screen = host.wait_ready().await;
    assert!(screen.contains("narrow"), "{screen}");
    assert!(
        screen.lines().all(|line| line.chars().count() <= 36),
        "nothing is drawn past the right edge:\n{screen}"
    );
    assert!(
        !screen.contains(STARTUP_MODEL),
        "below 40 columns the meta row drops host facts:\n{screen}"
    );

    host.resize(160, 30);
    let screen = host.frame().await.join("\n");
    assert!(
        screen.contains("code ❯ a fairly long answer that has to wrap on a narrow terminal"),
        "{screen}"
    );
    assert!(screen.contains(STARTUP_MODEL), "{screen}");
}
