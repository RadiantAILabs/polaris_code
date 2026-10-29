//! Consumer proof for backgrounded shell commands and the task controls.
//!
//! The composition under test is the interactive one: the real conversation
//! panel with a `TurnInputHandle` and a `with_background_handle`, the real
//! command registry behind the real `CommandDispatch`, the real `CodeAgent`
//! turn, the real `ShellExecutor`, and the real `BackgroundReporter`. Only the
//! model is scripted — there is no key and no network — and the panel is driven
//! with `TuiEvent`s instead of a crossterm loop. Everything in between is
//! production code, which is the point: the defect this design is most exposed
//! to is a host that only works in a harness.
//!
//! Every wait is bounded by a named condition rather than by a fixed interval,
//! so a passing run returns on the first frame that satisfies it and a broken
//! one fails with the screen it actually had.
//!
//! The binding properties each test covers are named on the test.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::shell::{ShellConfig, ShellExecutor, ShellPlugin, ShellProcessState};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, PermissionMode, PolicySnapshot,
};
use polaris_code_core::config::ModelCatalog;
use polaris_code_core::run;
use polaris_code_core::terminal::background::{
    BackgroundCommands, BackgroundReporter, BackgroundTasks, CommandResult, Reconciliation,
    TaskView, background_receipt, deadline_clause,
};
use polaris_code_core::terminal::{self, CommandRegistry, SessionSettings, TurnSteering};
use polaris_code_core::tools::TaskToolsPlugin;
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiEvent, TuiPanel, TuiRegistry};

const MODEL: &str = "scripted/claude-sonnet-4-6";

/// Maximum time to observe an asynchronous condition settle.
///
/// A passing path returns on the first frame that matches. The bound exists so
/// genuinely broken behaviour fails instead of hanging the suite; load can
/// delay an observation inside the bound, but cannot make an unmet condition
/// pass.
const SETTLE_DEADLINE: Duration = Duration::from_secs(20);
/// Polling cadence while waiting for a condition to settle.
const SETTLE_POLL: Duration = Duration::from_millis(20);

const USAGE: Usage = Usage {
    input_tokens: Some(10),
    output_tokens: Some(2),
    total_tokens: Some(12),
    cache_read_tokens: None,
    cache_creation_tokens: None,
    reasoning_output_tokens: None,
};

// ─────────────────────────────────────────────────────────────────────────────
// The scripted model
// ─────────────────────────────────────────────────────────────────────────────

/// What the model does on one call.
#[derive(Debug, Clone)]
enum Step {
    /// Ask for a tool, by name and arguments.
    Call {
        id: String,
        name: String,
        arguments: String,
    },
    /// Answer with text and end the turn.
    Text(String),
}

/// A model whose every reply is written by the test.
///
/// Requests are kept so a test can prove what the model was — and was not —
/// asked. `Text("done")` is the default once the script runs out, so a turn
/// always terminates rather than looping on an empty script.
#[derive(Clone, Default)]
struct ScriptedProvider {
    script: Arc<Mutex<Vec<Step>>>,
    seen: Arc<Mutex<Vec<LlmRequest>>>,
}

impl ScriptedProvider {
    fn next(&self, request: LlmRequest) -> Step {
        self.seen.lock().unwrap().push(request);
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            Step::Text("done".into())
        } else {
            script.remove(0)
        }
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(&self, _: &str, request: LlmRequest) -> Result<LlmResponse, GenerationError> {
        let content = match self.next(request) {
            Step::Text(text) => vec![AssistantBlock::text(text)],
            Step::Call {
                id,
                name,
                arguments,
            } => vec![AssistantBlock::tool_call(
                polaris::models::llm::ToolCall::new(
                    id,
                    name,
                    serde_json::from_str(&arguments).unwrap(),
                ),
            )],
        };
        Ok(LlmResponse {
            content,
            usage: USAGE,
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let (block, delta, stop) = match self.next(request) {
            Step::Text(text) => (
                ContentBlockStartData::Text,
                ContentBlockDelta::Text(text),
                StopReason::EndTurn,
            ),
            Step::Call {
                id,
                name,
                arguments,
            } => (
                ContentBlockStartData::ToolCall {
                    id,
                    call_id: None,
                    name,
                },
                ContentBlockDelta::ToolCall { arguments },
                StopReason::ToolUse,
            ),
        };
        let events = vec![
            Ok(StreamEvent::ContentBlockStart { index: 0, block }),
            Ok(StreamEvent::ContentBlockDelta { index: 0, delta }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: stop,
                usage: USAGE,
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct ScriptedModelPlugin {
    provider: ScriptedProvider,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::background_tasks_model";
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

/// Provides the `TuiRegistry` capability the way `TuiPlugin::build` does, so
/// the contributed panel stays in the registry for the test to drive.
struct HeadlessTuiPlugin;

impl Plugin for HeadlessTuiPlugin {
    const ID: &'static str = "test::headless_tui_background";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fixture commands
// ─────────────────────────────────────────────────────────────────────────────

/// Write `body` as a shell script under `dir` and return the command to run it.
///
/// Fixtures are files rather than inline command strings on purpose: the
/// approval dialog renders the command verbatim, so an inline `echo first-line`
/// would put the very text a test asserts on the *output* onto the screen
/// before the command had run at all.
fn fixture(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(format!("{name}.sh"));
    std::fs::write(&path, body).unwrap();
    format!("sh {}", path.display())
}

/// A fixture body that appends one line to `marker` every time it runs.
///
/// Counting the lines is how "exactly one spawn ever happened" is proved:
/// a design that re-ran the command instead of abandoning the wait would write
/// a second line, and no amount of transcript inspection would show it.
fn spawn_counting(marker: &Path, rest: &str) -> String {
    format!("echo ran >> {}\n{rest}\n", marker.display())
}

fn marker_runs(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .map(|text| text.lines().filter(|line| !line.is_empty()).count())
        .unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// The interactive host, wired as `main.rs` wires it
// ─────────────────────────────────────────────────────────────────────────────

struct Host {
    panel: Box<dyn TuiPanel>,
    sessions: SessionsAPI,
    session_id: SessionId,
    provider: ScriptedProvider,
    tasks: BackgroundTasks,
    executor: ShellExecutor,
    chord_presses: Arc<AtomicUsize>,
    terminal: Terminal<TestBackend>,
    _root: tempfile::TempDir,
    working_dir: PathBuf,
}

impl Host {
    /// Boot with the executor's own output bound, and overflow files on.
    async fn boot(timeout_secs: u64) -> Self {
        Self::boot_with(timeout_secs, None).await
    }

    /// Boot the interactive host with a shell whose default deadline is
    /// `timeout_secs` and, when given, a small in-memory capture bound so the
    /// executor's overflow path is exercised. The model script is set
    /// afterwards, because a fixture command has to be written into the
    /// working directory this creates.
    async fn boot_with(timeout_secs: u64, max_output_bytes: Option<usize>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let working_dir = root.path().join("repo");
        std::fs::create_dir_all(&working_dir).unwrap();
        let working_dir = working_dir.canonicalize().unwrap();

        let provider = ScriptedProvider::default();
        let config = AgentConfig::new(MODEL, working_dir.clone());
        let commands = Arc::new(CommandRegistry::with_builtins());
        let steering = TurnSteering::enabled();
        let mut shell_config = ShellConfig::new()
            .with_working_dir(&working_dir)
            .with_allowed_dirs(vec![working_dir.clone()])
            .with_allowed_commands(vec!["*".into()])
            .with_timeout(timeout_secs);
        shell_config = match max_output_bytes {
            Some(bytes) => shell_config
                .with_max_output_bytes(bytes)
                .with_cache_dir(root.path().join("overflow")),
            None => shell_config.disable_cache(),
        };

        let (tasks, mut backgrounded) = BackgroundTasks::new(Duration::from_secs(timeout_secs));
        let chord_presses = Arc::new(AtomicUsize::new(0));

        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                provider: provider.clone(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(ShellPlugin::new(shell_config))
            // The model's half of the same surface, over the same executor and
            // the same table. Present in every test in this file on purpose:
            // the chord and `/command` proofs below are also the proof that
            // adding the model's route did not disturb the user's.
            .add_plugins(TaskToolsPlugin::new(tasks.clone()))
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                working_dir.join(".polaris/sessions"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(HeadlessTuiPlugin);

        let chord_handle = Arc::new(std::sync::OnceLock::<BackgroundCommands>::new());
        let chord_target = chord_handle.clone();
        let chord_count = chord_presses.clone();
        let panel_plugin = ConversationPanelPlugin::default()
            .with_turn_input_handle(Arc::new(steering.clone()))
            .with_background_handle(move || {
                chord_count.fetch_add(1, Ordering::SeqCst);
                let Some(commands) = chord_target.get().cloned() else {
                    return;
                };
                tokio::spawn(async move { commands.request_background().await });
            });
        let (conversation, output) =
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

        let executor = server
            .get_global::<ShellExecutor>()
            .map(|executor| ShellExecutor::clone(&executor))
            .expect("ShellPlugin publishes the executor");

        let settings =
            SessionSettings::new(MODEL, PermissionMode::Confirm, &ModelCatalog::pinned(MODEL));

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let session_id = SessionId::from_string("background");
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
                    let tasks = tasks.clone();
                    move |ctx| {
                        ctx.insert(config.clone());
                        ctx.insert(settings.clone());
                        ctx.insert(steering.clone());
                        ctx.insert(tasks.clone());
                    }
                },
            )
            .unwrap();
        // The production posture, unmodified: every attended mode resolves
        // `run_command` to `Confirm`, so each test below approves the command
        // the way a user does. That is not incidental to this feature — it is
        // why backgrounding can only ever be offered after the gate.
        sessions
            .with_context(&session_id, |ctx| {
                ctx.insert(PolicySnapshot::defaults());
            })
            .await
            .unwrap();

        let current = session_id.clone();
        let background =
            BackgroundCommands::new(executor.clone(), tasks.clone(), output.clone(), move || {
                current.clone()
            });
        background.register(&commands).unwrap();
        let _ = chord_handle.set(background);

        let current = session_id.clone();
        let reporter = BackgroundReporter::new(
            executor.clone(),
            output,
            steering.clone(),
            tasks.clone(),
            move || current.clone(),
        );
        tokio::spawn(async move {
            while let Some(number) = backgrounded.recv().await {
                let reporter = reporter.clone();
                tokio::spawn(async move {
                    reporter.watch(number).await;
                });
            }
        });

        Self {
            panel,
            sessions,
            session_id,
            provider,
            tasks,
            executor,
            chord_presses,
            terminal: Terminal::new(TestBackend::new(160, 40)).unwrap(),
            _root: root,
            working_dir,
        }
    }

    /// Write the model's script. Set before the turn starts.
    fn script(&self, steps: Vec<Step>) {
        *self.provider.script.lock().unwrap() = steps;
    }

    /// A fixture script in this host's working directory, as a command.
    fn fixture(&self, name: &str, body: &str) -> String {
        fixture(&self.working_dir, name, body)
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

    /// Draw one frame now. Instantaneous on purpose — only `frame_when`
    /// asserts on a frame, and it is the one that names a condition.
    fn draw_frame(&mut self) -> String {
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

    /// The first frame satisfying `settled`, or a failure carrying the last one.
    async fn frame_when(&mut self, description: &str, settled: impl Fn(&str) -> bool) -> String {
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            let screen = self.draw_frame();
            if settled(&screen) {
                return screen;
            }
            assert!(
                tokio::time::Instant::now() < expiry,
                "{description}; last frame was:\n{screen}"
            );
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    /// Wait for a non-screen condition, keeping the panel pumping while it does.
    async fn settle_when(&mut self, description: &str, ready: impl Fn(&Self) -> bool) {
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            if ready(self) {
                return;
            }
            let screen = self.draw_frame();
            assert!(
                tokio::time::Instant::now() < expiry,
                "{description}; last frame was:\n{screen}"
            );
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }

    /// Pump frames until `driver` has finished, or fail.
    ///
    /// Reading canonical history takes the session's context lock, which a
    /// running turn holds, and the panel's IO channel is bounded — so a test
    /// that stopped drawing and waited on the lock would stall the very turn
    /// it was waiting for. Draining first is what keeps that impossible.
    async fn drain_turn(&mut self, driver: tokio::task::JoinHandle<()>) {
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        while !driver.is_finished() {
            let screen = self.draw_frame();
            assert!(
                tokio::time::Instant::now() < expiry,
                "the turn did not finish; last frame was:\n{screen}"
            );
            tokio::time::sleep(SETTLE_POLL).await;
        }
        let _ = driver.await;
    }

    /// Start one real turn, parked at the production input boundary.
    fn start_turn(&self) -> tokio::task::JoinHandle<()> {
        let sessions = self.sessions.clone();
        let id = self.session_id.clone();
        tokio::spawn(async move {
            run::drive_turn(&sessions, &id).await;
        })
    }

    /// Submit a prompt the way a user does.
    fn submit(&mut self, prompt: &str) {
        self.type_text(prompt);
        self.key(KeyCode::Enter, KeyModifiers::NONE);
    }

    /// Answer the approval dialog the way a user does.
    ///
    /// Not a shortcut around the gate: the dialog is the gate, and pressing `y`
    /// here is what makes everything after it a post-approval operation.
    async fn approve(&mut self) {
        self.frame_when("the approval dialog opens", |s| s.contains("approve?"))
            .await;
        self.type_text("y");
        self.key(KeyCode::Enter, KeyModifiers::NONE);
        self.frame_when("the dialog closes on the answer", |s| {
            !s.contains("approve?")
        })
        .await;
    }

    /// Run a whole `/command` through the real dispatch path.
    ///
    /// `line` is the command without its slash. Typing the slash first and
    /// waiting for the popup is not decoration: the panel treats a leading `/`
    /// as a command only once the host has advertised that it runs them, and
    /// the host advertises from `CommandDispatch::receive` — so this also
    /// proves the command set is live, which is the thing a turn parked on its
    /// steering queue used to starve.
    async fn run_slash(&mut self, line: &str) {
        self.type_text("/");
        self.frame_when("the host advertises its command set", |s| {
            s.contains("List the available commands")
        })
        .await;
        self.type_text(line);
        // Dismiss the completion popup so Enter submits the literal line
        // rather than accepting a highlighted entry.
        self.key(KeyCode::Esc, KeyModifiers::NONE);
        self.key(KeyCode::Enter, KeyModifiers::NONE);
    }

    /// Everything the model has been handed, as canonical messages.
    async fn history(&self) -> Vec<Message> {
        self.sessions
            .with_context(&self.session_id, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .unwrap()
                    .messages
                    .clone()
            })
            .await
            .unwrap()
    }

    /// Every `tool_result` in canonical history, as `(call id, text)`.
    async fn tool_results(&self) -> Vec<(String, String)> {
        self.history()
            .await
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::ToolResult(result) => Some((
                            result.id.clone(),
                            match &result.content {
                                polaris::models::llm::ToolResultContent::Text(text) => text.clone(),
                                other => format!("{other:?}"),
                            },
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                Message::Assistant { .. } => Vec::new(),
            })
            .collect()
    }

    /// Every user-authored text block in canonical history.
    async fn user_texts(&self) -> Vec<String> {
        self.history()
            .await
            .iter()
            .flat_map(|message| match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                Message::Assistant { .. } => Vec::new(),
            })
            .collect()
    }

    fn only_task(&self) -> TaskView {
        let all = self.tasks.all();
        assert_eq!(all.len(), 1, "expected exactly one tracked task: {all:?}");
        all[0].clone()
    }

    fn state_of(&self, task: &TaskView) -> Option<ShellProcessState> {
        self.executor.process(task.id).map(|status| status.state)
    }
}

/// One `run_command` step for `command`, under call id `id`.
fn call(id: &str, command: &str) -> Step {
    Step::Call {
        id: id.into(),
        name: "run_command".into(),
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

/// One `run_in_background` step for `command`, under call id `id`.
fn background_call(id: &str, command: &str) -> Step {
    Step::Call {
        id: id.into(),
        name: "run_in_background".into(),
        arguments: serde_json::json!({ "command": command }).to_string(),
    }
}

/// One `task_output` step naming `task`, under call id `id`.
fn output_call(id: &str, task: u64) -> Step {
    Step::Call {
        id: id.into(),
        name: "task_output".into(),
        arguments: serde_json::json!({ "task": task }).to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 1 — live output reaches the block before the command is terminal
// ─────────────────────────────────────────────────────────────────────────────

/// Partial output, and the `ToolContext` switch itself: output the host can
/// only have received through the sink shows up in the block the invocation
/// opened, in one block, while the process is still running.
#[tokio::test]
async fn live_output_reaches_one_block_before_the_command_finishes() {
    let mut host = Host::boot(60).await;
    let command = host.fixture("live", "echo alpha-out\nsleep 5\necho omega-out\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;

    let screen = host
        .frame_when(
            "the first line reaches the block while the command runs",
            |s| s.contains("alpha-out"),
        )
        .await;
    assert_eq!(
        screen.matches("run_command").count(),
        1,
        "live output updates one block in place rather than appending rows:\n{screen}"
    );
    assert!(
        !screen.contains("omega-out"),
        "the second line has not been printed yet, so it must not be on screen:\n{screen}"
    );
    let task = host.only_task();
    assert_eq!(
        host.state_of(&task),
        Some(ShellProcessState::Running),
        "the process is genuinely still running when its output was rendered"
    );

    driver.abort();
    let _ = driver.await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 2 — backgrounding abandons the wait and nothing else
// ─────────────────────────────────────────────────────────────────────────────

/// No re-run, one call with one result, and a receipt that names the
/// deadline, on the real chord.
#[tokio::test]
async fn backgrounding_abandons_the_wait_without_re_running_or_duplicating() {
    let marker = tempfile::NamedTempFile::new().unwrap();
    let mut host = Host::boot(120).await;
    let command = host.fixture("counted", &spawn_counting(marker.path(), "sleep 30"));
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;

    host.settle_when("the command starts and is tracked", |host| {
        host.tasks.all().len() == 1
    })
    .await;
    let task = host.only_task();
    assert_eq!(marker_runs(marker.path()), 1, "the command spawned once");

    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let screen = host
        .frame_when("the block reports the task it became", |s| {
            s.contains(&format!("backgrounded as task {}", task.number))
        })
        .await;
    assert!(
        screen.contains("left of its"),
        "the receipt names the remaining deadline:\n{screen}"
    );
    assert_eq!(
        host.chord_presses.load(Ordering::SeqCst),
        1,
        "one press, one request"
    );

    // The OS process was untouched. Nothing restarted it, and nothing
    // spawned a second one.
    assert_eq!(
        host.state_of(&task),
        Some(ShellProcessState::Running),
        "the wait was handed to the host, not dropped, so the child kept running"
    );
    assert_eq!(
        marker_runs(marker.path()),
        1,
        "backgrounding re-ran nothing: exactly one spawn ever happened"
    );

    // The invocation closed once, with a normal result naming the task.
    host.settle_when("the turn moves past the tool call", |_| true)
        .await;
    let results = host.tool_results().await;
    let for_call: Vec<_> = results.iter().filter(|(id, _)| id == "c1").collect();
    assert_eq!(
        for_call.len(),
        1,
        "exactly one tool_result for the call id: {results:?}"
    );
    assert!(
        for_call[0]
            .1
            .contains(&format!("Backgrounded as task {}", task.number)),
        "and it names the task: {:?}",
        for_call[0].1
    );
    assert!(
        !for_call[0].1.contains("error"),
        "it is a normal result, not an error: {:?}",
        for_call[0].1
    );

    driver.abort();
    let _ = driver.await;
    host.executor.stop(task.id).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 3 — list, inspect, follow and stop
// ─────────────────────────────────────────────────────────────────────────────

/// `/tasks` names the deadline, states the platform guarantee and implies no
/// PTY, and `/stop` has typed outcomes.
#[tokio::test]
async fn tasks_can_be_listed_inspected_followed_and_stopped() {
    let mut host = Host::boot(120).await;
    let command = host.fixture("listed", "echo listening-now\nsleep 30\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    // The turn that ran the command has ended. Park a fresh one at the input
    // boundary, which is where the host advertises and dispatches commands.
    driver.abort();
    let _ = driver.await;
    let driver = host.start_turn();

    host.run_slash("tasks").await;
    let screen = host
        .frame_when("the task list names the running task", |s| {
            s.contains("/tasks ❯")
        })
        .await;
    assert!(
        screen.contains("running"),
        "the state is reported:\n{screen}"
    );
    assert!(
        screen.contains("left of its"),
        "and the remaining deadline:\n{screen}"
    );
    assert!(
        screen.contains("process group") || screen.contains("direct child"),
        "the platform process-tree guarantee is stated:\n{screen}"
    );
    assert!(
        screen.contains("not terminals"),
        "and the list does not imply a PTY:\n{screen}"
    );

    host.run_slash(&format!("task {}", task.number)).await;
    let screen = host
        .frame_when("the task detail shows the retained tail", |s| {
            s.contains("listening-now")
        })
        .await;
    assert!(
        screen.contains("not the whole of it"),
        "a tail is never presented as complete output:\n{screen}"
    );

    host.run_slash(&format!("fg {}", task.number)).await;
    let screen = host
        .frame_when("following says what it does and does not do", |s| {
            s.contains("Following task")
        })
        .await;
    assert!(
        screen.contains("does not reopen"),
        "/fg re-attaches attention, not the tool call:\n{screen}"
    );

    host.run_slash(&format!("stop {}", task.number)).await;
    host.frame_when("the stop is acknowledged", |s| s.contains("Stopping task"))
        .await;
    host.settle_when("the process reaches a terminal state", |host| {
        !matches!(
            host.state_of(&task),
            Some(ShellProcessState::Running) | Some(ShellProcessState::Terminating)
        )
    })
    .await;

    // A second stop is a typed refusal reporting the real reason, never a
    // silent false.
    host.run_slash(&format!("stop {}", task.number)).await;
    let screen = host
        .frame_when("the second stop is refused in words", |s| {
            s.contains("already finished")
                || s.contains("already stopping")
                || s.contains("no task")
        })
        .await;
    assert!(
        screen.contains("cancelled")
            || screen.contains("already stopping")
            || screen.contains("no task"),
        "the refusal names the real outcome:\n{screen}"
    );

    // An unknown id is refused the same way.
    host.run_slash("stop 987654").await;
    host.frame_when("an unknown id is refused", |s| s.contains("no task 987654"))
        .await;

    driver.abort();
    let _ = driver.await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 4 — completion reconciles mid-turn, and discloses when it cannot
// ─────────────────────────────────────────────────────────────────────────────

/// Both directions: a completion arriving while a turn is running reaches
/// `ContextManager` through the steering boundary, and one arriving with the
/// session idle does **not** — it renders the transcript-only disclosure
/// instead, and starts no turn.
#[tokio::test]
async fn an_idle_completion_is_transcript_only_and_says_so() {
    let mut host = Host::boot(120).await;
    let command = host.fixture("quick", "echo done-quickly\nsleep 2\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    // Let the turn finish, so the completion lands with nothing to steer.
    driver.abort();
    let _ = driver.await;
    let calls_before = host.provider.calls();

    let screen = host
        .frame_when("the completion closes the block", |s| {
            s.contains(&format!("task {} finished", task.number))
        })
        .await;
    assert!(
        screen.contains("transcript only"),
        "an idle completion discloses that it is transcript-only:\n{screen}"
    );

    let texts = host.user_texts().await;
    assert!(
        !texts.iter().any(|text| text.contains("has finished")),
        "the completion did not reach the model's history: {texts:?}"
    );
    assert_eq!(
        host.provider.calls(),
        calls_before,
        "and no turn was started to report it"
    );
}

/// The other direction: with a turn genuinely in flight, the completion goes
/// through the graph-owned steering boundary and *is* in canonical history.
#[tokio::test]
async fn a_mid_turn_completion_reaches_canonical_history() {
    // Two shell calls. The first is backgrounded and finishes quickly; the
    // second keeps a turn running long enough for the completion to be steered
    // into it at a safe boundary.
    let mut host = Host::boot(120).await;
    let first = host.fixture("first", "echo quick-task\nsleep 2\n");
    let second = host.fixture("second", "sleep 6\n");
    host.script(vec![
        call("c1", &first),
        call("c2", &second),
        Step::Text("finished".into()),
    ]);
    let driver = host.start_turn();
    host.submit("run both");
    host.approve().await;
    host.settle_when("the first command starts", |host| {
        host.tasks.all().len() == 1
    })
    .await;
    let task = host.only_task();
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the first task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    // The turn continues into the second command, and the first finishes under
    // it — which is the only state in which the steering boundary exists.
    host.approve().await;
    let screen = host
        .frame_when("the completion closes the backgrounded block", |s| {
            s.contains(&format!("task {} finished", task.number))
        })
        .await;
    assert!(
        !screen.contains("transcript only"),
        "a steered completion is in history, so it must not claim otherwise:\n{screen}"
    );

    host.drain_turn(driver).await;
    let texts = host.user_texts().await;
    assert!(
        texts.iter().any(
            |text| text.contains(&format!("Background task {}", task.number))
                && text.contains("has finished")
        ),
        "the mid-turn route reaches ContextManager, so the model was told: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text.contains("quick-task")),
        "and it carried the command's own output: {texts:?}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 5 — every refusal is typed and explainable
// ─────────────────────────────────────────────────────────────────────────────

/// The chord is answered in words in every case it
/// cannot be honoured, including the one where honouring it would admit an
/// unapproved command to the background.
#[tokio::test]
async fn every_background_refusal_is_typed_and_explainable() {
    let mut host = Host::boot(120).await;
    host.script(vec![Step::Text("nothing to do".into())]);

    // Nothing running at all.
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let screen = host
        .frame_when("an idle host refuses in words", |s| {
            s.contains("nothing is running to background")
        })
        .await;
    assert!(
        screen.contains("Ctrl-T"),
        "and says what the chord applies to:\n{screen}"
    );

    // A tool that is not a shell command.
    let mut host = Host::boot(120).await;
    host.script(vec![Step::Call {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: serde_json::json!({ "path": "missing.txt" }).to_string(),
    }]);
    // `read_file` is not registered by this server, so the registry refuses it
    // — which is fine: what is proved here is the slot, not the tool. Press the
    // chord while the block is open.
    let driver = host.start_turn();
    host.submit("read it");
    host.settle_when("the invocation is announced", |host| {
        host.tasks.by_call("c1").is_none()
    })
    .await;
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("a non-shell tool is refused in words", |s| {
        s.contains("only shell commands can be backgrounded")
            || s.contains("nothing is running to background")
    })
    .await;
    driver.abort();
    let _ = driver.await;

    // The block is parked on the approval dialog.
    // No policy change needed: `run_command` already resolves to `Confirm` in
    // every attended mode, which is exactly the state under test.
    let mut host = Host::boot(120).await;
    let command = host.fixture("gated", "sleep 30\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.frame_when("the approval dialog is open", |s| {
        s.contains("approve") || s.contains("Approve")
    })
    .await;
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    let screen = host
        .frame_when("an unapproved command cannot be backgrounded", |s| {
            s.contains("answer the approval first")
        })
        .await;
    assert!(
        screen.contains("unapproved"),
        "the refusal says why:\n{screen}"
    );
    assert!(
        host.tasks.all().is_empty(),
        "and nothing was started by asking"
    );

    driver.abort();
    let _ = driver.await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 6 — turn cancellation leaves a backgrounded task alone
// ─────────────────────────────────────────────────────────────────────────────

/// Stopping a *turn* and stopping a *process* are different operations on
/// different objects, and neither is routed through the other.
#[tokio::test]
async fn cancelling_the_turn_does_not_kill_a_backgrounded_task() {
    let mut host = Host::boot(120).await;
    let first = host.fixture("one", "sleep 30\n");
    let second = host.fixture("two", "sleep 30\n");
    host.script(vec![call("c1", &first), call("c2", &second)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the first command starts", |host| {
        host.tasks.all().len() == 1
    })
    .await;
    let backgrounded = host.only_task();
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;
    // The turn moves on to a second command, which asks for its own approval.
    host.approve().await;
    host.settle_when("the second command is running in the foreground", |host| {
        host.tasks.all().len() == 2
    })
    .await;

    // Interrupt the turn the way the host does.
    run::request_interrupt(&host.sessions, &host.session_id.clone());
    host.settle_when(
        "the foreground command is terminated by the turn signal",
        |host| {
            host.tasks
                .all()
                .iter()
                .filter(|task| task.number != backgrounded.number)
                .all(|task| !matches!(host.state_of(task), Some(ShellProcessState::Running)))
        },
    )
    .await;

    assert_eq!(
        host.state_of(&backgrounded),
        Some(ShellProcessState::Running),
        "the backgrounded task is no longer part of the turn, so the turn's signal must not reach it"
    );

    driver.abort();
    let _ = driver.await;
    host.executor.stop(backgrounded.id).await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 7 — the deadline is honoured and reported as itself
// ─────────────────────────────────────────────────────────────────────────────

/// A task that ends at its deadline reports `timed out`, not
/// a bare failure and not a claim that the user stopped it.
#[tokio::test]
async fn a_task_that_reaches_its_deadline_reports_timing_out() {
    let mut host = Host::boot(2).await;
    let command = host.fixture("slow", "echo starting\nsleep 30\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();
    assert!(
        deadline_clause(&task).contains("2s timeout"),
        "the configured deadline is what is reported: {}",
        deadline_clause(&task)
    );
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    let screen = host
        .frame_when(
            "the deadline ends the task and is named as the cause",
            |s| s.contains("timed out at its"),
        )
        .await;
    assert!(
        screen.contains("2s deadline"),
        "the deadline it hit is the one that was announced:\n{screen}"
    );
    assert!(
        !screen.contains("was stopped\n") && !screen.contains("stopped by"),
        "a timeout is not reported as a user stop:\n{screen}"
    );

    driver.abort();
    let _ = driver.await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Nothing is silently dropped, and a partial view says it is partial
// ─────────────────────────────────────────────────────────────────────────────

/// A coalesced tail is never presented as complete output, and when the
/// executor's in-memory capture overflows the block names the file that has
/// the whole of it.
#[tokio::test]
async fn a_partial_view_of_the_output_says_so_and_names_the_overflow_file() {
    // 1 KiB of capture against ~64 KiB of output: the executor keeps a bounded
    // prefix in memory and writes the complete stream to its overflow file.
    let mut host = Host::boot_with(120, Some(1024)).await;
    let command = host.fixture(
        "loud",
        "i=0\nwhile [ $i -lt 2000 ]; do echo \"line $i padding padding padding\"; i=$((i+1)); done\nsleep 2\n",
    );
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();
    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    let screen = host
        .frame_when("the completion reports how the view is partial", |s| {
            s.contains(&format!("task {} finished", task.number))
        })
        .await;
    assert!(
        screen.contains("capture overflowed"),
        "the block says the in-memory capture was not the whole output:\n{screen}"
    );
    assert!(
        screen.contains("complete output is on disk at"),
        "and names the file that is complete:\n{screen}"
    );
    assert!(
        screen.contains("overflow"),
        "the named path is the executor's own overflow file:\n{screen}"
    );

    driver.abort();
    let _ = driver.await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof 8 — the executor's reservation, in both directions
// ─────────────────────────────────────────────────────────────────────────────

/// The upstream premise the host's shape rests on, stated where a change to
/// it will be noticed: `execute` holds the run's handle inside its wait, and
/// dropping that wait stops the child. This is why backgrounding carries the
/// wait in a task of its own instead of dropping it — and why an unreserved
/// `start` would not have been enough on its own either. If this proof ever
/// fails, the design's premise moved and `ToolRun` should be revisited, not
/// patched around.
#[tokio::test]
async fn dropping_a_foreground_wait_stops_its_child() {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".into()])
            .disable_sandbox()
            .disable_cache(),
    );
    let observer = executor.clone();
    let wait = executor.execute(polaris::shell::ShellRequest::new("sleep 30"));
    let id = tokio::select! {
        _ = wait => panic!("a 30 s command does not finish first"),
        id = async {
            loop {
                if let Some(status) = observer.processes().into_iter().next() {
                    break status.id;
                }
                tokio::time::sleep(SETTLE_POLL).await;
            }
        } => id,
    };
    // The `select!` dropped the `execute` future along with the handle inside
    // it, so the run is no longer `Running`.
    let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        let state = executor.process(id).map(|status| status.state);
        if !matches!(state, Some(ShellProcessState::Running)) {
            assert!(
                matches!(
                    state,
                    Some(ShellProcessState::Terminating)
                        | Some(ShellProcessState::Finished(
                            polaris::shell::ShellOutcome::Cancelled
                        ))
                ),
                "the dropped wait requested a stop, got {state:?}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < expiry,
            "the child kept running after its wait was dropped"
        );
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

/// Upstream reserves a foreground run's result for the `execute` caller inside
/// `run_command` — `ShellExecutor::wait_for` refuses it — and stops the child
/// when that caller's handle is dropped. Backgrounding therefore cannot be a
/// dropped wait any more: the caller has to stay alive, and the result has to
/// come from it. What this proves is the observable half of that: the run is
/// reserved before and after the chord, an outside waiter still gets nothing,
/// the child is still running, and the real exit status still closes the block.
#[tokio::test]
async fn a_backgrounded_result_reaches_its_watcher_without_leaving_reservation() {
    let mut host = Host::boot(120).await;
    let command = host.fixture("coded", "echo working\nsleep 2\nexit 3\n");
    host.script(vec![call("c1", &command)]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();
    assert!(
        host.executor.process(task.id).is_some_and(|s| s.reserved),
        "a run_command run is reserved for its foreground caller from the start"
    );

    host.key(KeyCode::Char('t'), KeyModifiers::CONTROL);
    host.frame_when("the task is backgrounded", |s| {
        s.contains("backgrounded as task")
    })
    .await;

    // Backgrounding moved the wait, not the ownership: the run is still
    // reserved, so an outside waiter — anything holding the server-scoped
    // executor — is refused rather than handed the result.
    assert!(
        host.executor.process(task.id).is_some_and(|s| s.reserved),
        "the run stays reserved after backgrounding"
    );
    let stolen = tokio::time::timeout(SETTLE_DEADLINE, host.executor.wait_for(task.id))
        .await
        .expect("wait_for on a reserved run answers without waiting for the command");
    assert!(
        stolen.is_none(),
        "an outside waiter must be refused, got {stolen:?}"
    );
    assert_eq!(
        host.state_of(&task),
        Some(ShellProcessState::Running),
        "abandoning the turn's wait did not stop the child"
    );

    let screen = host
        .frame_when(
            "the watcher closes the block with the real exit status",
            |s| s.contains(&format!("task {} finished: exit 3", task.number)),
        )
        .await;
    assert!(
        !screen.contains("already been taken"),
        "the result reached its watcher rather than being reported lost:\n{screen}"
    );
    host.settle_when("the finished task leaves the table", |host| {
        host.tasks.all().is_empty()
    })
    .await;

    driver.abort();
    let _ = driver.await;
}

/// The other direction, on the real turn: while `run_command` waits on its
/// command in the foreground, an outside `wait_for` on the same run is refused
/// instead of racing the tool for the result, and the model is handed the
/// command's own exit status. A repair that started the run unreserved would
/// pass every other proof in this file and fail this one.
#[tokio::test]
async fn a_foreground_result_cannot_be_taken_from_under_the_tool() {
    let mut host = Host::boot(120).await;
    let command = host.fixture("owned", "echo held\nsleep 2\nexit 4\n");
    host.script(vec![call("c1", &command), Step::Text("finished".into())]);
    let driver = host.start_turn();
    host.submit("run it");
    host.approve().await;
    host.settle_when("the command starts", |host| host.tasks.all().len() == 1)
        .await;
    let task = host.only_task();

    let status = host
        .executor
        .process(task.id)
        .expect("the foreground run is discoverable through the executor");
    assert!(
        status.reserved,
        "a foreground run_command run must be reserved for the tool: {status:?}"
    );
    let stolen = tokio::time::timeout(SETTLE_DEADLINE, host.executor.wait_for(task.id))
        .await
        .expect("wait_for on a reserved run answers without waiting for the command");
    assert!(
        stolen.is_none(),
        "the outside waiter must be refused, got {stolen:?}"
    );
    assert_eq!(
        host.state_of(&task),
        Some(ShellProcessState::Running),
        "asking did not disturb the run"
    );

    host.drain_turn(driver).await;
    let results = host.tool_results().await;
    let (_, text) = results
        .iter()
        .find(|(id, _)| id == "c1")
        .expect("the invocation closed with a tool_result");
    // Rendered as shell text with an exit marker, not as the JSON response
    // document — see `tools/shell_render.rs`.
    assert!(
        text.contains("[exit code: 4]"),
        "the tool received its own result, exit 4: {text}"
    );
    assert!(
        !text.contains("consumed") && !text.contains("already been taken"),
        "and not a report that the result had been taken: {text}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// The rendering contracts, unit-checked
// ─────────────────────────────────────────────────────────────────────────────

/// The transcript-only wording, for each route that cannot reach the model's history, and the
/// silence of the one that can.
#[tokio::test]
async fn only_a_transcript_only_completion_carries_the_disclosure() {
    let task = sample_task().await;
    assert_eq!(Reconciliation::Steered.disclosure(&task), "");
    for how in [
        Reconciliation::TranscriptOnly,
        Reconciliation::ForeignSession,
        Reconciliation::Uncontained,
    ] {
        let text = how.disclosure(&task);
        assert!(
            text.contains("transcript only"),
            "{how:?} must say what it is: {text}"
        );
    }
    assert!(
        Reconciliation::ForeignSession
            .disclosure(&task)
            .contains(task.session.as_str()),
        "a foreign-session completion names the session that owns it"
    );
}

/// The deadline wording, at the one surface a test cannot press a key to reach: the receipt text
/// itself names the remaining deadline and refuses the model permission to
/// re-run the command.
#[tokio::test]
async fn the_receipt_names_the_deadline_and_forbids_a_re_run() {
    let task = sample_task().await;
    let receipt = background_receipt(&task);
    assert!(
        receipt.contains(&format!("task {}", task.number)),
        "{receipt}"
    );
    assert!(receipt.contains("timeout"), "{receipt}");
    assert!(receipt.contains("do not re-run"), "{receipt}");
    assert!(
        !receipt.to_lowercase().contains("failed"),
        "a backgrounded call is not a failure: {receipt}"
    );
}

/// The reporter reads a completion out of the `executed` response the tool
/// returns, because the run's `ExecutionResult` is reserved for the tool and
/// the struct itself cannot be built or parsed from outside `polaris_shell`.
/// This pins that read to upstream's own serialization of a real result, and
/// checks the one response shape that never started a run is refused by name.
#[tokio::test]
async fn a_completion_is_read_back_from_the_tools_own_executed_response() {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".into()])
            .disable_sandbox()
            .disable_cache(),
    );
    let polaris::shell::ShellResponse::Executed(result) = executor
        .execute(polaris::shell::ShellRequest::new("exit 7"))
        .await
        .unwrap()
    else {
        panic!("an allowed command needs no confirmation");
    };
    let response =
        serde_json::to_value(polaris::shell::ShellToolResponse::Executed(result.clone())).unwrap();
    let read = CommandResult::from_tool_response(&response).expect("an executed response reads");
    assert_eq!(read.exit_code, Some(7));
    assert_eq!(read.outcome, result.outcome);
    assert_eq!(read.duration, result.duration);
    assert_eq!(read.dropped_events, result.dropped_events);
    assert_eq!(read.cleanup_completed, result.cleanup_completed);
    assert_eq!(read.stdout_truncated, result.stdout_truncated);
    assert_eq!(read.stdout_overflow_path, result.stdout_overflow_path);

    let refused = CommandResult::from_tool_response(&serde_json::json!({
        "status": "confirmation_required",
        "command": "rm -rf build",
    }))
    .expect_err("a response that never started a run is not a completion");
    assert!(
        refused.contains("confirmation_required"),
        "the refusal names the response it got: {refused}"
    );
}

/// A view over a real run identity.
///
/// [`ShellProcessId`] is deliberately opaque — it cannot be constructed or
/// parsed from outside `polaris_shell` — so the only honest way to hold one is
/// to run something. `true` is the cheapest something there is.
async fn sample_task() -> TaskView {
    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(vec!["*".into()])
            .disable_sandbox()
            .disable_cache(),
    );
    let polaris::shell::ShellResponse::Executed(result) = executor
        .execute(polaris::shell::ShellRequest::new("true"))
        .await
        .unwrap()
    else {
        panic!("an allowed command needs no confirmation");
    };
    TaskView {
        number: polaris_code_core::terminal::background::task_number(result.process),
        id: result.process,
        call_id: "c1".into(),
        parent: None,
        label: "run_command".into(),
        command: "cargo test".into(),
        session: SessionId::from_string("main"),
        turn: 1,
        elapsed: Duration::from_secs(60),
        remaining: Duration::from_secs(60),
        deadline: Duration::from_secs(120),
        last_line: None,
        tail: vec!["compiling".into()],
        dropped: 0,
        capture_overflowed: false,
        backgrounded: true,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Proof — the model's own route out of the turn
// ─────────────────────────────────────────────────────────────────────────────

/// The requirement itself: a command the *model* backgrounds returns control to
/// the model before the command finishes.
///
/// Nothing about the turn is shortened to make this true — the turn runs to its
/// own completion, and the process is still running when it does. A harness
/// that only had the `Ctrl-T` chord could not pass this at all: there is no
/// user here, and no key is pressed after the approval.
#[tokio::test]
async fn the_model_can_background_a_command_and_the_turn_ends_while_it_runs() {
    let mut host = Host::boot(60).await;
    let command = host.fixture("slow", "sleep 10\necho too-late\n");
    host.script(vec![
        background_call("c1", &command),
        Step::Text("started it; carrying on".into()),
    ]);
    let driver = host.start_turn();
    host.submit("build it");
    // Gated exactly as the foreground command is, so the model's route begins
    // where the user's does: at the dialog.
    host.approve().await;
    host.settle_when("the task reaches the table", |host| {
        !host.tasks.all().is_empty()
    })
    .await;
    let task = host.only_task();
    host.drain_turn(driver).await;

    assert_eq!(
        host.state_of(&task),
        Some(ShellProcessState::Running),
        "the turn finished while the command was still running"
    );
    let results = host.tool_results().await;
    let (_, receipt) = results
        .iter()
        .find(|(id, _)| id == "c1")
        .expect("the backgrounding call closed with a tool result");
    assert!(
        receipt.contains(&format!("task {}", task.number)),
        "the model is told the task number it must read back with: {receipt}"
    );
    assert!(
        receipt.contains("Do not re-run"),
        "the receipt tells the model the command is still going: {receipt}"
    );
    assert!(
        !host
            .tool_results()
            .await
            .iter()
            .any(|(_, text)| text.contains("too-late")),
        "the command's own output cannot have been waited for"
    );

    let _ = host.executor.stop(task.id).await;
}

/// The other half of the requirement: the model reads the output and the exit
/// status back, for a command that finished while no turn was active.
///
/// The gap this closes is precise. `ShellExecutor::wait_for` consumes a
/// terminal result exactly once and `ShellProcessStatus` carries no exit code,
/// so before the retained ring existed, a completion that landed between turns
/// reached the transcript and nothing else — the model could never learn it.
/// Here the first turn ends, the process finishes with nobody asking, and the
/// second turn gets both the output and the code.
#[tokio::test]
async fn the_model_reads_back_a_task_that_finished_between_turns() {
    let mut host = Host::boot(60).await;
    let command = host.fixture("later", "sleep 1\necho beta-out\nexit 7\n");
    host.script(vec![
        background_call("c1", &command),
        Step::Text("started".into()),
    ]);
    let first = host.start_turn();
    host.submit("run the suite");
    host.approve().await;
    // The dialog closing is the user's answer, not the spawn: the tool has not
    // called `start` yet, so the table is still empty for a moment.
    host.settle_when("the task reaches the table", |host| {
        !host.tasks.all().is_empty()
    })
    .await;
    let number = host.only_task().number;
    host.drain_turn(first).await;

    // No turn is running now. The command finishes here, into nothing.
    host.settle_when("the task finishes while the session is idle", |host| {
        host.tasks.finished_by_number(number).is_some()
    })
    .await;
    assert!(
        host.tasks.all().is_empty(),
        "a finished run leaves the live list, exactly as it did before"
    );

    host.script(vec![
        output_call("c2", number),
        Step::Text("it failed with 7".into()),
    ]);
    let second = host.start_turn();
    host.submit("what did it do?");
    host.drain_turn(second).await;

    let results = host.tool_results().await;
    let (_, read_back) = results
        .iter()
        .find(|(id, _)| id == "c2")
        .expect("the read-back call returned a result");
    assert!(
        read_back.contains("beta-out"),
        "the model gets the output it never waited for: {read_back}"
    );
    assert!(
        read_back.contains("exited with code 7"),
        "the model gets the exit status: {read_back}"
    );
    assert!(
        read_back.contains("bytes"),
        "the read-back says how much of the stream it is showing: {read_back}"
    );
}

/// A backgrounded command is gated exactly as the same command in the
/// foreground: same dialog, and a denial starts no process at all.
///
/// Backgrounding is a scheduling choice. If it were also a permission choice,
/// the cheapest way past a `Confirm` would be to ask for the background.
#[tokio::test]
async fn backgrounding_is_gated_exactly_as_the_foreground_command_is() {
    let mut host = Host::boot(60).await;
    let marker = host.working_dir.join("ran.txt");
    let command = host.fixture("denied", &spawn_counting(&marker, "sleep 10"));
    host.script(vec![
        background_call("c1", &command),
        Step::Text("refused".into()),
    ]);
    let driver = host.start_turn();
    host.submit("run it in the background");

    // The same dialog the foreground command opens, naming the same command.
    let screen = host
        .frame_when("the approval dialog opens for the background call", |s| {
            s.contains("approve?")
        })
        .await;
    assert!(
        screen.contains("run_in_background"),
        "the dialog names the tool that asked: {screen}"
    );
    host.type_text("n");
    host.key(KeyCode::Enter, KeyModifiers::NONE);
    host.drain_turn(driver).await;

    assert_eq!(
        marker_runs(&marker),
        0,
        "a denied background command never spawned"
    );
    assert!(
        host.tasks.all().is_empty() && host.tasks.finished().is_empty(),
        "and left no task behind"
    );
    let results = host.tool_results().await;
    let (_, refusal) = results
        .iter()
        .find(|(id, _)| id == "c1")
        .expect("the denied call still closed exactly once");
    assert!(
        refusal.contains("User denied execution"),
        "the model is told it was refused, in the same words: {refusal}"
    );
}

/// Every shipped permission mode answers the same for the foreground and the
/// background route to a shell command.
///
/// The integration test above proves one mode through the real dialog; this
/// proves the table itself cannot drift, including for a mode added later that
/// forgot to name the second tool.
#[test]
fn every_mode_gates_both_shell_routes_identically() {
    use polaris::tools::ToolPermission;
    use polaris_code_core::agents::UnattendedGrants;

    let modes = [
        PermissionMode::Confirm,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
        PermissionMode::Unattended(UnattendedGrants::default()),
        PermissionMode::Unattended(UnattendedGrants {
            edit_file: true,
            run_command_patterns: vec!["cargo *".into()],
        }),
    ];
    for mode in modes {
        let policy = mode.permission_policy();
        // Resolved against the widest possible registry default, so a name the
        // table forgot would show up as `Allow` rather than being masked.
        let foreground = policy.resolve("run_command", ToolPermission::Allow);
        let background = policy.resolve("run_in_background", ToolPermission::Allow);
        assert_eq!(
            foreground,
            background,
            "{} gates run_in_background differently from run_command",
            mode.label()
        );
    }
}
