//! Consumer proof for the managed shell runtime, in two parts.
//!
//! **Part 1 — the real coding permission path.** A scripted `LlmProvider` asks
//! for `run_command` with a controlled command and a scripted `IOProvider`
//! answers the confirm dialog, so one real turn runs through the binary's own
//! `receive_user_input → act → execute_tools` graph with no model key and no
//! network. The command's only observable effect is an append to a marker file,
//! so "one process ran" and "no process ran" are counted from the filesystem
//! rather than inferred from a log line; the executor's monotonic
//! `ShellProcessId` is read back as a second, independent witness that a denied
//! call allocates no run identity at all.
//!
//! The gate under test is the one `execute_tools` honours: the `run_command`
//! entry in the session's `PolicySnapshot`. It is installed there rather than on
//! the `ToolRegistry` because the registry is frozen into a global before the
//! composition root gets another look at it — see
//! [`a_registry_gate_cannot_be_installed_after_the_server_is_finished`].
//!
//! **Part 2 — the streaming contract through `ToolRegistry::execute_with`.**
//! The same `run_command` tool, reached through the registry the way the agent
//! reaches it, but with a bounded `ChannelSink` and a watch-backed `ShellCancel`
//! in the `ToolContext`: incremental stdout/stderr, timeout, cancellation, drop
//! accounting, bounded capture with a complete overflow file, and Unix
//! descendant cleanup.
//!
//! What this file deliberately does **not** prove: integration with the turn
//! cancellation token. `execute_tools` still calls `ToolRegistry::execute` with
//! an empty context, so the sink and cancel observers below are supplied by the
//! test, not by the agent. Carrying the real turn token and the host sink into
//! `execute_tools` is covered by the cancellation and terminal-background tests.

use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolCall,
    ToolResultContent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    CONFIRMED, CONFIRMED_FALSE, CONFIRMED_TRUE, IO_TYPE, IO_TYPE_CONFIRM, IOContent, IOError,
    IOMessage, IOProvider, PersistenceAPI, PersistencePlugin, UserIO,
};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::shell::{
    ChannelSink, ShellCancel, ShellConfig, ShellEvent, ShellExecutor, ShellPermission, ShellPlugin,
    ShellRequest, ShellSinkHandle, ShellStream,
};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolContext, ToolPermission, ToolRegistry, ToolsPlugin};
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager, PolicySnapshot,
};
use polaris_code_core::run::{self, TurnReason};
use polaris_code_core::terminal::transcript::{
    IO_TYPE_TOOL, META_LABEL, META_PHASE, PHASE_DENIED, PHASE_ERROR, PHASE_OK,
};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use serde_json::{Value, json};
use tokio::sync::watch;

// ─────────────────────────────────────────────────────────────────────────────
// The production shell composition
// ─────────────────────────────────────────────────────────────────────────────

/// Destructive patterns the binary refuses outright.
///
/// Mirrors `DENIED_COMMANDS` in `src/main.rs`: a binary's `const` is not
/// reachable from an integration test, so the proof carries the same list and
/// asserts the policy it produces.
const DENIED_COMMANDS: &[&str] = &["rm -rf *", "rm -rf /", "sudo *", ":(){ :|:& };:", "mkfs *"];

/// The binary's `ShellConfig`, built exactly the way `src/main.rs` builds it —
/// including the two composition fixes it carries: the working directory is
/// granted to the sandbox, and the inner command policy allows, so that the one
/// outer confirmation reaches execution instead of being asked for twice.
///
/// One deviation, marked here because it is the test's and not the binary's:
/// `cache_dir` is redirected into `cache`. The default is `~/.polaris`, which a
/// test has no business creating.
fn production_shell_config(working_dir: &Path, cache: &Path) -> ShellConfig {
    ShellConfig::new()
        .with_working_dir(working_dir)
        .with_timeout(120)
        .with_max_output_bytes(64 * 1024)
        .with_allowed_dirs(vec![working_dir.to_path_buf()])
        .with_allowed_commands(vec!["*".to_string()])
        .with_denied_commands(DENIED_COMMANDS.iter().map(|s| (*s).to_string()).collect())
        .with_cache_dir(cache)
}

#[test]
fn production_policy_defers_to_the_outer_gate_but_keeps_deny_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let working_dir = dir.path().canonicalize().unwrap();
    let config = production_shell_config(&working_dir, cache.path());

    assert!(
        config.sandbox,
        "the binary must not opt out of the directory sandbox"
    );
    assert!(
        config.allowed_dirs.contains(&working_dir),
        "the working directory has to be granted explicitly, or an approved \
         command is rejected before it can spawn: {:?}",
        config.allowed_dirs
    );

    let executor = ShellExecutor::new(config);
    assert_eq!(
        executor.check_permission("printf hello"),
        ShellPermission::Allow,
        "an ordinary command must not be gated a second time inside the shell"
    );
    for denied in [
        "sudo rm /etc/hosts",
        "rm -rf /",
        "printf hi && sudo rm /etc/hosts",
    ] {
        assert_eq!(
            executor.check_permission(denied),
            ShellPermission::Deny,
            "deny patterns stay authoritative for {denied:?}"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 1 — the real coding permission path
// ─────────────────────────────────────────────────────────────────────────────

/// The tool-call id the scripted model uses; the transcript keys its activity
/// block on it.
const CALL_ID: &str = "call-run-command-1";

/// The text the scripted model answers with once the tool result comes back,
/// which ends the `ReAct` loop.
const FINAL_ANSWER: &str = "done.";

/// An `LlmProvider` that asks for `run_command` once and then stops.
///
/// Turn shape: first call → one `run_command` tool call with `command`; every
/// later call → plain text, so `has_tool_calls` routes to `finalize` and the
/// loop terminates. No key, no network.
struct ScriptedProvider {
    /// The command the model asks to run.
    command: String,
    /// How many times the agent has called the model this turn.
    calls: AtomicUsize,
}

impl ScriptedProvider {
    /// The assistant message for this call, tool call first.
    fn next_response(&self) -> LlmResponse {
        let first = self.calls.fetch_add(1, Ordering::Relaxed) == 0;
        let content = if first {
            vec![AssistantBlock::tool_call(ToolCall::new(
                CALL_ID,
                "run_command",
                json!({ "command": self.command }),
            ))]
        } else {
            vec![AssistantBlock::text(FINAL_ANSWER)]
        };
        LlmResponse {
            content,
            usage: Usage::default(),
            stop_reason: if first {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            },
            id: None,
            model: None,
        }
    }
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
        Ok(self.next_response())
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let response = self.next_response();
        let mut events = Vec::new();
        for block in response.content {
            match block {
                AssistantBlock::Text(text) => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::Text,
                    }));
                    events.push(Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::Text(text.text),
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                }
                AssistantBlock::ToolCall(call) => {
                    events.push(Ok(StreamEvent::ContentBlockStart {
                        index: 0,
                        block: ContentBlockStartData::ToolCall {
                            id: call.id,
                            call_id: None,
                            name: call.function.name,
                        },
                    }));
                    events.push(Ok(StreamEvent::ContentBlockDelta {
                        index: 0,
                        delta: ContentBlockDelta::ToolCall {
                            arguments: call.function.arguments.to_string(),
                        },
                    }));
                    events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                }
                AssistantBlock::Reasoning(_) => {}
            }
        }
        events.push(Ok(StreamEvent::MessageStop {
            stop_reason: response.stop_reason,
            usage: Usage::default(),
            id: None,
            model: None,
        }));
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] with [`ModelRegistry`] through the same seam
/// `AnthropicPlugin` uses.
struct ScriptedModelPlugin {
    /// The command the scripted model asks to run.
    command: String,
}

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
            .register_llm_provider(ScriptedProvider {
                command: self.command.clone(),
                calls: AtomicUsize::new(0),
            });
    }
}

/// What the scripted user does when the agent opens a confirm dialog.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// Stamp the reply `confirmed: true`, as an approving front end does.
    Approve,
    /// Stamp the reply `confirmed: false`, as a denying front end does.
    Deny,
}

/// Mutable state behind [`ScriptedIOProvider`].
#[derive(Default)]
struct ScriptedIOState {
    /// The turn's prompt, taken by the first `receive`.
    prompt: Option<String>,
    /// Set by the outbound confirm request, cleared by the reply.
    confirm_open: bool,
    /// Every message the agent sent, in order.
    sent: Vec<IOMessage>,
}

/// A scripted front end: hands the agent one prompt, then answers each confirm
/// dialog the same way, and records everything the agent sends back.
///
/// This is the `UserIO` side of the real permission path — `execute_tools`
/// cannot tell it from `TuiIOProvider` or `ExecIOProvider`, because it answers
/// the same `confirm` request with the same `confirmed` metadata.
struct ScriptedIOProvider {
    /// How the scripted user answers every confirm dialog.
    answer: Answer,
    /// Prompt, dialog state, and the outbound transcript.
    state: Mutex<ScriptedIOState>,
}

impl ScriptedIOProvider {
    /// Creates a provider that delivers `prompt` once and answers with `answer`.
    fn new(prompt: &str, answer: Answer) -> Self {
        Self {
            answer,
            state: Mutex::new(ScriptedIOState {
                prompt: Some(prompt.to_owned()),
                ..ScriptedIOState::default()
            }),
        }
    }

    /// The text of every tool-activity message that reached the given phase.
    fn tool_messages(&self, phase: &str) -> Vec<String> {
        let state = self.state.lock().expect("scripted IO lock poisoned");
        state
            .sent
            .iter()
            .filter(|message| {
                message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_TOOL)
                    && message.metadata.get(META_PHASE).map(String::as_str) == Some(phase)
                    && message.metadata.get(META_LABEL).map(String::as_str) == Some("run_command")
            })
            .map(|message| match &message.content {
                IOContent::Text(text) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// How many confirm dialogs the agent opened.
    fn confirm_prompts(&self) -> Vec<String> {
        let state = self.state.lock().expect("scripted IO lock poisoned");
        state
            .sent
            .iter()
            .filter(|message| {
                message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM)
            })
            .map(|message| match &message.content {
                IOContent::Text(text) => text.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }
}

impl IOProvider for ScriptedIOProvider {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        let mut state = self.state.lock().expect("scripted IO lock poisoned");
        if message.metadata.get(IO_TYPE).map(String::as_str) == Some(IO_TYPE_CONFIRM) {
            state.confirm_open = true;
        }
        state.sent.push(message);
        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        let mut state = self.state.lock().expect("scripted IO lock poisoned");
        if state.confirm_open {
            state.confirm_open = false;
            let confirmed = match self.answer {
                Answer::Approve => CONFIRMED_TRUE,
                Answer::Deny => CONFIRMED_FALSE,
            };
            return Ok(IOMessage::user_text("answer").with_metadata(CONFIRMED, confirmed));
        }
        // The turn's one prompt, then end-of-input — never a hang.
        state.prompt.take().map_or(Err(IOError::Closed), |prompt| {
            Ok(IOMessage::from_external("test", IOContent::Text(prompt)))
        })
    }
}

/// Installs the scripted front end exactly the way `ExecIOPlugin` installs its
/// own: one `register_local` factory over a shared provider.
struct ScriptedIOPlugin {
    /// The shared provider the test also reads its transcript back from.
    provider: Arc<ScriptedIOProvider>,
}

impl Plugin for ScriptedIOPlugin {
    const ID: &'static str = "test::scripted_io";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let provider = self.provider.clone();
        server.register_local(move || UserIO::new(provider.clone()));
    }
}

/// One coding turn, wired as `main.rs` wires it apart from the scripted model
/// and front end: the same plugin set, the same `ShellConfig`, and the same
/// `run_command` permission installed into the session's `PolicySnapshot`.
struct CodingTurn {
    /// The finished server; owns the APIs and the shell executor.
    server: Server,
    /// The scripted front end's transcript.
    io: Arc<ScriptedIOProvider>,
    /// The working directory the shell is sandboxed to.
    working_dir: PathBuf,
    /// The `run_command` permission the coding policy installs for this turn.
    permission: ToolPermission,
    /// Marker file the controlled command appends one line to per run.
    marker: PathBuf,
    /// Kept alive for the duration of the turn.
    _workspace: tempfile::TempDir,
    /// Kept alive for the duration of the turn.
    _data: tempfile::TempDir,
    /// Kept alive for the duration of the turn.
    _cache: tempfile::TempDir,
}

impl CodingTurn {
    /// Builds the turn. `command_template` is what the scripted model asks to
    /// run (`{marker}` expands to the marker path); `permission` is the
    /// `run_command` policy permission `main.rs` installs.
    async fn build(command_template: &str, answer: Answer, permission: ToolPermission) -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let working_dir = workspace.path().canonicalize().unwrap();
        let marker = working_dir.join("ran.log");
        let command = command_template.replace("{marker}", &marker.display().to_string());

        let io = Arc::new(ScriptedIOProvider::new("run it", answer));
        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(ScriptedModelPlugin {
                command: command.clone(),
            })
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(ShellPlugin::new(production_shell_config(
                &working_dir,
                cache.path(),
            )))
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                data.path().to_str().expect("a UTF-8 temp path"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(ScriptedIOPlugin {
                provider: io.clone(),
            });
        // `act` reads the session token meter through `Res<TokenMeter>`; the
        // binary registers it as a global before `finish()`, and so must this.
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        Self {
            server,
            io,
            working_dir,
            permission,
            marker,
            _workspace: workspace,
            _data: data,
            _cache: cache,
        }
    }

    /// Drives exactly one turn through the production run surface, with the
    /// binary's coding permission policy applied the same way `main.rs` applies
    /// it: into the session context, after the agent's `setup` has seeded the
    /// defaults.
    async fn drive(&self) -> run::TurnOutcome {
        self.drive_session().await.0
    }

    /// [`Self::drive`], keeping the session id so the caller can read back the
    /// canonical history the model was handed.
    async fn drive_session(&self) -> (run::TurnOutcome, SessionId) {
        let sessions = self.server.api::<SessionsAPI>().unwrap();
        let session_id = SessionId::new();
        sessions
            .create_session_with_executor(
                self.server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
                {
                    let config = AgentConfig::new("scripted/test-model", self.working_dir.clone());
                    move |ctx| {
                        ctx.insert(config.clone());
                    }
                },
            )
            .unwrap();

        let mut policy = PolicySnapshot::defaults();
        policy
            .permission
            .overrides
            .insert("run_command".to_string(), self.permission);
        sessions
            .with_context(&session_id, move |ctx| {
                ctx.insert(policy);
            })
            .await
            .unwrap();

        // The registry itself is untouched — its `run_command` entry is still
        // the `#[tool]` default. The gate the agent honours is the policy
        // override above, which is the whole reason it has to exist.
        assert_eq!(
            self.server
                .get_global::<ToolRegistry>()
                .expect("the registry is a global once the server is finished")
                .permission("run_command"),
            Some(ToolPermission::Allow),
        );

        let outcome = run::drive_turn(sessions, &session_id).await;
        (outcome, session_id)
    }

    /// The text of every `tool_result` the model was handed in `session`.
    ///
    /// This is the model's side of the boundary, not the front end's: it reads
    /// the canonical messages the next `act` would send, so it sees exactly the
    /// bytes the provider does.
    async fn tool_result_texts(&self, session: &SessionId) -> Vec<String> {
        self.server
            .api::<SessionsAPI>()
            .unwrap()
            .with_context(session, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .unwrap()
                    .messages
                    .iter()
                    .flat_map(|message| match message {
                        Message::User { content } => content
                            .iter()
                            .filter_map(|block| match block {
                                UserBlock::ToolResult(result) => match &result.content {
                                    ToolResultContent::Text(text) => Some(text.clone()),
                                    other => Some(format!("{other:?}")),
                                },
                                _ => None,
                            })
                            .collect::<Vec<_>>(),
                        Message::Assistant { .. } => Vec::new(),
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap()
    }

    /// The shell executor the server built from the production config.
    fn executor(&self) -> ShellExecutor {
        (*self.server.get_global::<ShellExecutor>().unwrap()).clone()
    }

    /// How many times the controlled command actually ran.
    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.marker)
            .map(|text| text.lines().filter(|line| *line == "ran").count())
            .unwrap_or(0)
    }

    /// The identity the executor would hand the *next* accepted run.
    ///
    /// `ShellProcessId` is monotonic per executor, so this is a witness that is
    /// independent of the marker file: an identity is allocated only once a run
    /// has passed every gate, so a `1` here means nothing was ever accepted.
    async fn next_process_id(&self) -> u64 {
        let result = self
            .executor()
            .execute_confirmed(ShellRequest::new("true"))
            .await
            .expect("a probe command runs inside the sandbox");
        serde_json::to_value(result.process)
            .expect("ShellProcessId serializes")
            .as_u64()
            .expect("ShellProcessId is a number")
    }
}

/// The controlled command: appends one line per execution and nothing else.
const MARKER_COMMAND: &str = "printf 'ran\\n' >> '{marker}'";

#[tokio::test]
async fn approved_run_command_runs_exactly_one_process() {
    let turn = CodingTurn::build(MARKER_COMMAND, Answer::Approve, ToolPermission::Confirm).await;
    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(
        turn.io.confirm_prompts().len(),
        1,
        "the user is asked exactly once: the inner shell policy must not open a \
         second dialog"
    );
    assert_eq!(
        turn.runs(),
        1,
        "approval must reach execution exactly once, not zero times and not twice"
    );

    let results = turn.io.tool_messages(PHASE_OK);
    assert_eq!(results.len(), 1, "one completed tool block: {results:?}");
    let result: Value = serde_json::from_str(&results[0]).expect("the tool result is JSON");
    assert_eq!(result["status"], "executed", "{result}");
    assert_eq!(result["exit_code"], 0, "{result}");
    assert_eq!(result["outcome"], "exited", "{result}");
    assert_eq!(
        result["process"], 1,
        "the approved run is the executor's first accepted identity: {result}"
    );
    assert!(
        result["cleanup_completed"].as_bool().unwrap_or(false),
        "{result}"
    );

    // Nothing is left live or retained: `execute` consumed the one result.
    assert!(turn.executor().processes().is_empty());
}

/// A command that writes to both streams and fails, so one result has to carry
/// all three things the model needs to read: output, which stream it came
/// from, and whether the command succeeded.
const TWO_STREAM_FAILURE: &str = "printf 'line one\\nline two\\n'; printf 'boom\\n' >&2; exit 3";

#[tokio::test]
async fn a_shell_result_reaches_the_model_as_text_with_an_exit_marker() {
    let turn = CodingTurn::build(TWO_STREAM_FAILURE, Answer::Approve, ToolPermission::Allow).await;
    let (outcome, session) = turn.drive_session().await;
    assert_eq!(outcome.reason, TurnReason::Completed);

    let results = turn.tool_result_texts(&session).await;
    assert_eq!(results.len(), 1, "one shell result: {results:?}");
    let result = &results[0];

    // Text, not a JSON document the model has to unescape first.
    assert!(
        !result.contains("\\n"),
        "the model must not have to unescape newlines: {result}"
    );
    assert!(
        result.contains("line one\nline two"),
        "stdout arrives as the lines the command printed: {result}"
    );

    // The streams are distinguishable, and the exit status is unambiguous.
    assert!(
        result.contains("[stderr]") && result.contains("boom"),
        "stderr is present and marked as stderr: {result}"
    );
    assert!(
        result.contains("[exit code: 3]"),
        "the exit status is stated as a marker, not buried in a field: {result}"
    );

    // The bookkeeping fields the model cannot act on stay out of its window.
    for noise in ["cleanup_completed", "dropped_events", "\"process\""] {
        assert!(
            !result.contains(noise),
            "{noise} is executor bookkeeping, not something the model acts on: {result}"
        );
    }
}

#[tokio::test]
async fn a_silent_successful_command_still_says_it_succeeded() {
    let turn = CodingTurn::build("true", Answer::Approve, ToolPermission::Allow).await;
    let (outcome, session) = turn.drive_session().await;
    assert_eq!(outcome.reason, TurnReason::Completed);

    let results = turn.tool_result_texts(&session).await;
    assert_eq!(results.len(), 1, "one shell result: {results:?}");
    assert_eq!(
        results[0], "[no output; exit code: 0]",
        "a silent success is one unambiguous line"
    );
}

#[tokio::test]
async fn user_denial_runs_no_process() {
    let turn = CodingTurn::build(MARKER_COMMAND, Answer::Deny, ToolPermission::Confirm).await;
    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(turn.io.confirm_prompts().len(), 1, "the user was asked");
    assert_eq!(turn.runs(), 0, "a denied command must never execute");
    assert!(
        !turn.marker.exists(),
        "a denied command must not touch the filesystem at all"
    );

    let denials = turn.io.tool_messages(PHASE_DENIED);
    assert_eq!(denials.len(), 1, "one denied tool block: {denials:?}");
    assert!(
        denials[0].contains("User denied execution"),
        "the denial is attributed to the user: {denials:?}"
    );
    assert!(turn.io.tool_messages(PHASE_OK).is_empty());
    assert_eq!(
        turn.next_process_id().await,
        1,
        "a denied call must allocate no run identity"
    );
}

#[tokio::test]
async fn policy_denial_runs_no_process_and_never_asks() {
    let turn = CodingTurn::build(MARKER_COMMAND, Answer::Approve, ToolPermission::Deny).await;
    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert!(
        turn.io.confirm_prompts().is_empty(),
        "a denied tool is refused before the user is ever asked"
    );
    assert_eq!(turn.runs(), 0, "policy denial must never execute");

    let denials = turn.io.tool_messages(PHASE_DENIED);
    assert_eq!(denials.len(), 1, "one denied tool block: {denials:?}");
    assert!(
        denials[0].contains("is not allowed"),
        "the denial is attributed to policy: {denials:?}"
    );
    assert_eq!(
        turn.next_process_id().await,
        1,
        "a policy-denied call must allocate no run identity"
    );
}

#[tokio::test]
async fn denied_pattern_survives_user_approval() {
    // The user approves at the one gate that asks; the shell's deny list is
    // still authoritative, so the command never reaches spawn.
    let turn = CodingTurn::build(
        "sudo tee '{marker}'",
        Answer::Approve,
        ToolPermission::Confirm,
    )
    .await;
    let outcome = turn.drive().await;

    assert_eq!(outcome.reason, TurnReason::Completed);
    assert_eq!(turn.io.confirm_prompts().len(), 1, "the user approved");
    assert_eq!(turn.runs(), 0, "a denied pattern must never execute");

    let errors = turn.io.tool_messages(PHASE_ERROR);
    assert_eq!(errors.len(), 1, "one failed tool block: {errors:?}");
    assert!(
        errors[0].contains("Permission denied") && errors[0].contains("denied by policy"),
        "the refusal comes from the shell's deny list: {errors:?}"
    );
    assert_eq!(
        turn.next_process_id().await,
        1,
        "a pattern-denied call must allocate no run identity"
    );
}

#[tokio::test]
async fn a_registry_gate_cannot_be_installed_after_the_server_is_finished() {
    // Why the confirm gate lives in `PolicySnapshot` and not in a
    // `ToolRegistry::set_permission` call from the composition root:
    // `ToolsPlugin::ready` moves the registry out of the resource table and into
    // a global, so once `Server::finish()` has returned there is no mutable
    // handle left. A `set_permission` call there compiles, runs, and changes
    // nothing — leaving `run_command` on the macro default and the agent running
    // commands unprompted.
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(ShellPlugin::new(production_shell_config(
            dir.path(),
            cache.path(),
        )));
    server.finish().await.unwrap();

    assert!(
        server.get_resource_mut::<ToolRegistry>().is_none(),
        "there is nothing left for a post-finish `set_permission` to write to"
    );
    assert_eq!(
        server
            .get_global::<ToolRegistry>()
            .unwrap()
            .permission("run_command"),
        Some(ToolPermission::Allow),
        "unconfirmed execution is what an uninstalled gate leaves behind"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Part 2 — the streaming contract through `ToolRegistry::execute_with`
// ─────────────────────────────────────────────────────────────────────────────

/// A server carrying just the tool surface `run_command` needs, so the registry
/// under test is the same one the agent resolves through `Res<ToolRegistry>`.
async fn tool_server(config: ShellConfig) -> Server {
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(ShellPlugin::new(config));
    server.finish().await.unwrap();
    server
}

/// A permissive sandbox rooted at `dir`, with overflow files disabled.
fn sandboxed_config(dir: &Path) -> ShellConfig {
    ShellConfig::new()
        .with_working_dir(dir)
        .with_allowed_dirs(vec![dir.to_path_buf()])
        .with_allowed_commands(vec!["*".to_string()])
        .disable_cache()
}

/// `ToolContext` carrying the two `#[context]` values `run_command` accepts —
/// the seam the host will later use for the real turn token and host sink.
fn tool_context(sink: ChannelSink, cancel: ShellCancel) -> ToolContext {
    ToolContext::new()
        .with(ShellSinkHandle::new(sink))
        .with(cancel)
}

/// A non-zero sink capacity. `ChannelSink` takes a [`NonZeroUsize`] because a
/// zero-capacity sink could accept nothing at all.
fn sink_capacity(capacity: usize) -> NonZeroUsize {
    NonZeroUsize::new(capacity).expect("these tests all pass a positive capacity")
}

/// A watch-backed [`ShellCancel`] plus the sender that latches it.
fn watch_cancel() -> (watch::Sender<bool>, ShellCancel) {
    let (sender, receiver) = watch::channel(false);
    (sender, ShellCancel::from_watch(receiver))
}

#[tokio::test]
async fn streams_distinct_stdout_and_stderr_before_the_command_exits() {
    let dir = tempfile::tempdir().unwrap();
    let server = tool_server(sandboxed_config(dir.path())).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();

    let (sink, mut events) = ChannelSink::new(sink_capacity(32));
    let (_cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({
        "command": "printf 'out-one'; printf 'err-one' >&2; sleep 1; printf 'out-two'"
    });

    let observe = async {
        assert!(
            matches!(events.recv().await, Some(ShellEvent::Started { .. })),
            "the accepted run announces itself first"
        );
        // The command sleeps a full second in the middle; a chunk inside half
        // that window can only have come from incremental reading.
        let first = tokio::time::timeout(Duration::from_millis(500), events.recv())
            .await
            .expect("output must arrive before the command exits")
            .expect("a chunk, not end-of-stream");

        let (mut stdout, mut stderr, mut finished) = (false, false, false);
        let mut chunk = Some(first);
        while let Some(event) = chunk {
            match event {
                ShellEvent::Chunk { stream, .. } => {
                    stdout |= stream == ShellStream::Stdout;
                    stderr |= stream == ShellStream::Stderr;
                }
                ShellEvent::Finished { .. } => finished = true,
                // `ShellEvent` is `#[non_exhaustive]`; this observer only
                // counts the three facts the assertions below read.
                _ => {}
            }
            chunk = events.recv().await;
        }
        (stdout, stderr, finished)
    };

    let (value, (stdout, stderr, finished)) =
        tokio::join!(registry.execute_with("run_command", &args, &ctx), observe);

    let result = value.expect("the approved command runs");
    assert_eq!(result["status"], "executed", "{result}");
    assert_eq!(result["stdout"], "out-oneout-two", "{result}");
    assert_eq!(result["stderr"], "err-one", "{result}");
    assert_eq!(result["dropped_events"], 0, "{result}");
    assert!(stdout && stderr, "both pipes stream distinctly");
    assert!(finished, "the terminal event closes the stream");
}

#[tokio::test]
async fn a_full_event_channel_drops_data_and_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    let server = tool_server(sandboxed_config(dir.path())).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();

    // Capacity one, forty writes: the data queue must overflow while the
    // reserved lifecycle slots stay observable.
    //
    // The counter is spelled as forty literal words rather than `$((i+1))`
    // because the executor downgrades an auto-allow to a confirmation for any
    // command carrying an opaque construct, and arithmetic expansion is one.
    // This test is about sink overflow, not about that gate.
    let (sink, mut events) = ChannelSink::new(sink_capacity(1));
    let (_cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({
        "command": "for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 \
                    21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 36 37 38 39 40; \
                    do printf x; sleep 0.005; done"
    });

    let result = registry
        .execute_with("run_command", &args, &ctx)
        .await
        .expect("the command runs");

    assert!(
        result["dropped_events"].as_u64().unwrap_or(0) > 0,
        "a sink that drops must be accounted for by the engine: {result}"
    );
    assert_eq!(result["stdout"], "x".repeat(40), "capture is unaffected");
    assert!(
        matches!(events.recv().await, Some(ShellEvent::Started { .. })),
        "Started survives a full data queue"
    );
    let mut last = None;
    while let Some(event) = events.recv().await {
        last = Some(event);
    }
    assert!(
        matches!(last, Some(ShellEvent::Finished { .. })),
        "Finished survives a full data queue and arrives last"
    );
}

#[tokio::test]
async fn timeout_returns_partial_output_and_truthful_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let config = sandboxed_config(dir.path()).with_terminate_grace(Duration::from_millis(50));
    let server = tool_server(config).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();

    let (sink, _events) = ChannelSink::new(sink_capacity(8));
    let (_cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({ "command": "printf partial; sleep 30", "timeout_secs": 1 });

    let result = registry
        .execute_with("run_command", &args, &ctx)
        .await
        .expect("a timeout is a result, not an error");

    assert_eq!(result["status"], "executed", "{result}");
    assert_eq!(result["outcome"], "timed_out", "{result}");
    assert_eq!(result["timed_out"], true, "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("partial"),
        "a timed-out command still returns what it printed: {result}"
    );
    assert_eq!(result["cleanup_completed"], true, "{result}");
}

#[tokio::test]
async fn a_latched_watch_signal_cancels_the_running_command() {
    let dir = tempfile::tempdir().unwrap();
    let config = sandboxed_config(dir.path()).with_terminate_grace(Duration::from_millis(50));
    let server = tool_server(config).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();

    let (sink, _events) = ChannelSink::new(sink_capacity(8));
    let (cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({ "command": "printf started; sleep 30" });

    let latch = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        cancel_tx.send(true).expect("the observer is still alive");
    };
    let (value, ()) = tokio::join!(registry.execute_with("run_command", &args, &ctx), latch);

    let result = value.expect("cancellation is a result, not an error");
    assert_eq!(result["outcome"], "cancelled", "{result}");
    assert_eq!(result["timed_out"], false, "{result}");
    assert_eq!(result["cleanup_completed"], true, "{result}");
    assert!(
        result["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("started"),
        "a cancelled command still returns what it printed: {result}"
    );
}

#[tokio::test]
async fn capture_stays_bounded_and_the_overflow_file_is_complete() {
    let dir = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let config = sandboxed_config(dir.path())
        .with_cache_dir(cache.path())
        .with_max_output_bytes(4);
    let server = tool_server(config).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();

    let (sink, mut events) = ChannelSink::new(sink_capacity(16));
    let (_cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({ "command": "printf 0123456789" });

    let result = registry
        .execute_with("run_command", &args, &ctx)
        .await
        .expect("the command runs");

    assert_eq!(result["stdout_truncated"], true, "{result}");
    assert_eq!(
        result["stdout"], "0123\n...[truncated, 4 byte limit]",
        "in-memory capture stops at the limit: {result}"
    );
    let overflow = PathBuf::from(
        result["stdout_overflow_path"]
            .as_str()
            .expect("a truncated stream has an overflow file"),
    );
    assert_eq!(
        tokio::fs::read(&overflow).await.unwrap(),
        b"0123456789",
        "the overflow file holds the complete stream, not only the excess"
    );

    let mut limit_reached = false;
    while let Some(event) = events.recv().await {
        if matches!(event, ShellEvent::CaptureLimitReached { .. }) {
            limit_reached = true;
        }
    }
    assert!(limit_reached, "the sink is told capture stopped");
}

/// Whether a process id still names a live (non-zombie) process.
///
/// Uses `kill -0` plus `ps`, so the check works on every Unix this crate is
/// built for without adding a `libc` dependency to the consumer.
#[cfg(unix)]
fn process_alive(pid: &str) -> bool {
    let signalled = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid} 2>/dev/null"))
        .status()
        .is_ok_and(|status| status.success());
    if !signalled {
        return false;
    }
    // A reaped-but-not-yet-collected zombie answers `kill -0` and is not a
    // surviving descendant.
    std::process::Command::new("ps")
        .args(["-o", "state=", "-p", pid])
        .output()
        .ok()
        .is_none_or(|out| {
            !String::from_utf8_lossy(&out.stdout)
                .trim_start()
                .starts_with('Z')
        })
}

/// How long a signalled descendant may take to disappear before the group
/// cleanup is judged to have failed.
///
/// `kill(2)` only marks a signal pending: the kernel tears the target down when
/// it next schedules it, so "the group was signalled" and "the descendant is
/// gone" are two events separated by scheduler latency. Five seconds is about
/// four orders of magnitude above the sub-millisecond gap an idle host shows,
/// which covers the kind of load that exposed this, while staying well inside
/// the pid-file deadline below — that one also waits out a fork and an exec,
/// whereas this waits only for the teardown of a process already signalled.
#[cfg(unix)]
const DESCENDANT_EXIT_DEADLINE: Duration = Duration::from_secs(5);

/// Whether `pid` stops naming a live process within `deadline`.
///
/// Returns as soon as the process is gone, so cleanup that worked costs one
/// [`process_alive`] check. A descendant that genuinely escaped the group is
/// never signalled at all, so it survives the whole `deadline` and the caller
/// still fails — the wait bounds the observation, it does not soften it.
#[cfg(unix)]
async fn exits_within(pid: &str, deadline: Duration) -> bool {
    let expiry = tokio::time::Instant::now() + deadline;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= expiry {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_cleans_up_the_descendant_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let config = sandboxed_config(dir.path()).with_terminate_grace(Duration::from_millis(200));
    let server = tool_server(config).await;
    let registry = server.get_global::<ToolRegistry>().unwrap();
    assert_eq!(
        ShellExecutor::tree_guarantee(),
        polaris::shell::ProcessTreeGuarantee::Group,
        "this proof only means anything where the group guarantee holds"
    );

    // A grandchild that ignores SIGTERM: only the process-group signal, escalated
    // to SIGKILL, can reach it.
    let pid_file = dir.path().join("descendant.pid");
    let command = format!(
        "sh -c 'trap \"\" TERM; while :; do sleep 1; done' & echo $! > '{}'; while :; do sleep 1; done",
        pid_file.display()
    );

    let (sink, _events) = ChannelSink::new(sink_capacity(8));
    let (cancel_tx, cancel) = watch_cancel();
    let ctx = tool_context(sink, cancel);
    let args = json!({ "command": command });

    // Cancel only once the descendant has recorded its pid, so the assertion
    // below is about group cleanup rather than about a race with startup.
    let supervise = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let pid = loop {
            let recorded = tokio::fs::read_to_string(&pid_file)
                .await
                .unwrap_or_default();
            let recorded = recorded.trim().to_owned();
            if !recorded.is_empty() {
                break recorded;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the fixture never wrote its pid file"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        // Instant on purpose, unlike the post-condition below: this asserts a
        // fact about the present moment, not the settled result of an
        // asynchronous kernel event.
        assert!(
            process_alive(&pid),
            "the descendant must be running before cancellation, or this proves \
             nothing"
        );
        cancel_tx.send(true).expect("the observer is still alive");
        pid
    };

    let (value, descendant) =
        tokio::join!(registry.execute_with("run_command", &args, &ctx), supervise);

    let result = value.expect("cancellation is a result, not an error");
    assert_eq!(result["outcome"], "cancelled", "{result}");
    assert_eq!(result["cleanup_completed"], true, "{result}");

    // `cleanup_completed` means the direct child was reaped and the group was
    // signalled; the executor cannot wait on a grandchild it does not own, and
    // `kill(2)` does not wait either. So the descendant's teardown lands after
    // `execute_with` returns, and observing it needs a bounded wait.
    assert!(
        exits_within(&descendant, DESCENDANT_EXIT_DEADLINE).await,
        "descendant {descendant} survived the group termination"
    );
}
