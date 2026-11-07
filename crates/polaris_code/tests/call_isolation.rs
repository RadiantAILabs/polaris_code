//! Several agent sessions on one server, each with its own prompt, folder,
//! command patterns and spend cap.
//!
//! A program that runs many agent calls in one process composes a single
//! server and starts one session per call (`src/docs/embedding.md`). What a
//! call may touch must therefore come from the call, not from the server:
//!
//! 1. **Roots.** The file tools and the shell work in the session's own folder
//!    ([`CallRoot`], filled from `AgentConfig::working_dir`), and refuse a path
//!    or a `working_dir` that leaves it. A toolset used without a `CallRoot`
//!    keeps the directory it was built with.
//! 2. **Command patterns.** A session's `command_globs` decide, per call, which
//!    shell commands run; an empty list leaves the session with no shell tool.
//! 3. **Role prompt.** `AgentConfig::system_prompt` replaces the built-in
//!    prompt in the request; the project grounding block stays.
//! 4. **Spend cap.** When the session's meter carries a cap, `act` reserves the
//!    worst case of each request before sending it and ends the call, without
//!    contacting the model, when the reservation does not fit.
//!
//! The model is scripted and nothing reaches a network.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt as _;
use polaris::models::llm::{
    ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider, LlmRequest,
    LlmResponse, LlmStream, Message, StopReason, StreamEvent, Usage,
};
use polaris::models::pricing::PriceStore;
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin, UserIO,
};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::shell::{ShellConfig, ShellExecutor, ShellPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{Tool, ToolContext, ToolsPlugin, Toolset};
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code::exec::ExecIOProvider;
use polaris_code::run::{self, TurnOutcome, TurnReason};
use polaris_code::tools::approval::{ApprovalShellPlugin, ApprovalShellTool};
use polaris_code::tools::{CallRoot, CodeTools, CodeToolsConfig, CodeToolsPlugin};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use serde_json::json;
use tokio::sync::Notify;

/// The provider name the scripted model registers under.
const PROVIDER: &str = "anthropic";

/// A model id the built-in price table knows: $2 in and $10 out per million
/// tokens.
const MODEL_ID: &str = "anthropic/claude-sonnet-5-5";

/// The start of the agent's built-in system prompt.
const BUILT_IN_PROMPT_START: &str =
    "You are a precise terminal coding agent operating in a sandboxed repository.";

/// What the scripted model says next.
enum Reply {
    /// One tool call, billed `input` and `output` tokens.
    Tool {
        name: &'static str,
        arguments: serde_json::Value,
        input: u64,
        output: u64,
    },
    /// A text answer, billed `input` and `output` tokens.
    Text {
        text: &'static str,
        input: u64,
        output: u64,
    },
    /// Signals `entered`, then never yields again.
    Stall { entered: Arc<Notify> },
    /// The provider refuses the request.
    Refuse,
}

fn tool(name: &'static str, arguments: serde_json::Value) -> Reply {
    Reply::Tool {
        name,
        arguments,
        input: 100,
        output: 10,
    }
}

fn text(text: &'static str) -> Reply {
    Reply::Text {
        text,
        input: 100,
        output: 10,
    }
}

/// One request the model received.
#[derive(Clone, Debug)]
struct Seen {
    system: String,
    /// The conversation as the model read it, in debug form so a test can
    /// search it for what a tool answered.
    messages: String,
    tools: Vec<String>,
    cache_prefix: bool,
    max_output_tokens: Option<u32>,
}

/// The replies still to give to the requests whose system prompt holds a needle.
type Script = (String, VecDeque<Reply>);

/// A scripted [`LlmProvider`]. Each script is picked by the text its needle
/// finds in the request's system prompt, which carries the session's folder,
/// so several sessions on one server follow their own scripts.
#[derive(Clone, Default)]
struct ScriptedProvider {
    scripts: Arc<Mutex<Vec<Script>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl ScriptedProvider {
    fn script(&self, needle: &str, replies: Vec<Reply>) {
        self.scripts
            .lock()
            .unwrap()
            .push((needle.to_string(), replies.into()));
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn requests_for(&self, needle: &str) -> Vec<Seen> {
        self.requests()
            .into_iter()
            .filter(|seen| seen.system.contains(needle))
            .collect()
    }
}

fn billed(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        total_tokens: Some(input + output),
        ..Default::default()
    }
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        PROVIDER
    }

    async fn generate(&self, _: &str, _: LlmRequest) -> Result<LlmResponse, GenerationError> {
        unreachable!("the agent streams its model calls")
    }

    async fn stream(&self, _: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let system = request.system.clone().unwrap_or_default();
        self.seen.lock().unwrap().push(Seen {
            system: system.clone(),
            messages: format!("{:?}", request.messages),
            tools: request
                .tools
                .iter()
                .flatten()
                .map(|tool| tool.name.clone())
                .collect(),
            cache_prefix: request.cache.prefix,
            max_output_tokens: request.max_output_tokens.map(std::num::NonZeroU32::get),
        });
        let reply = self
            .scripts
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(needle, _)| system.contains(needle.as_str()))
            .and_then(|(_, replies)| replies.pop_front())
            .expect("a scripted reply is left for this request");
        let stream: LlmStream = match reply {
            Reply::Tool {
                name,
                arguments,
                input,
                output,
            } => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: format!("{name}-call"),
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
                    usage: billed(input, output),
                    id: None,
                    model: None,
                }),
            ])
            .boxed(),
            Reply::Text {
                text,
                input,
                output,
            } => futures_util::stream::iter([
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text(text.to_string()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: billed(input, output),
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
            Reply::Refuse => {
                return Err(GenerationError::Auth("scripted refusal".into()));
            }
        };
        Ok(stream)
    }
}

struct ScriptedModelPlugin {
    provider: ScriptedProvider,
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
            .expect("ModelsPlugin builds first")
            .register_llm_provider(self.provider.clone());
    }
}

/// Destructive patterns the binary refuses outright (`DENIED_COMMANDS` in
/// `src/main.rs`; a binary's constant is not reachable from a test).
const DENIED_COMMANDS: &[&str] = &["rm -rf *", "rm -rf /", "sudo *", ":(){ :|:& };:", "mkfs *"];

/// One server, one scripted model, and a parent folder that holds every call's
/// folder.
struct Rig {
    _server: Server,
    sessions: SessionsAPI,
    provider: ScriptedProvider,
    /// The directory the file tools and the shell were built on.
    server_root: tempfile::TempDir,
    /// Holds each call's folder; the shell may enter anything beneath it.
    parent: tempfile::TempDir,
    /// The server's own meter. A call carries a meter of its own, so this one
    /// must stay at zero.
    global_meter: TokenMeter,
}

async fn rig() -> Rig {
    let server_root = tempfile::tempdir().unwrap();
    std::fs::write(server_root.path().join("SERVER-ROOT-MARKER.txt"), "server").unwrap();
    let parent = tempfile::tempdir().unwrap();
    let provider = ScriptedProvider::default();
    let global_meter = TokenMeter::new().with_pricing_store(PriceStore::builtin());

    let mut server = Server::new();
    server.insert_global(global_meter.clone());
    let shell = ShellConfig::new()
        .with_working_dir(server_root.path())
        .with_timeout(20)
        .with_max_output_bytes(64 * 1024)
        .with_allowed_dirs(vec![
            server_root.path().to_path_buf(),
            parent.path().to_path_buf(),
        ])
        .with_allowed_commands(vec!["*".to_string()])
        .with_denied_commands(DENIED_COMMANDS.iter().map(|s| (*s).to_string()).collect())
        .with_env_allowlist(vec!["PATH".to_string()])
        .disable_cache();
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin {
            provider: provider.clone(),
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(
            server_root.path(),
        )))
        .add_plugins(ShellPlugin::new(shell))
        .add_plugins(ApprovalShellPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
        .add_plugins(CodeAgentPlugin::new());
    server.finish().await.expect("the server starts");

    let persistence = server.api::<PersistenceAPI>().expect("persistence");
    let sessions = server.api::<SessionsAPI>().expect("sessions").clone();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).expect("agent registers");
    Rig {
        _server: server,
        sessions,
        provider,
        server_root,
        parent,
        global_meter,
    }
}

impl Rig {
    /// A new folder for one call, under the parent.
    fn folder(&self, name: &str) -> PathBuf {
        let folder = self.parent.path().join(name);
        std::fs::create_dir_all(&folder).unwrap();
        folder.canonicalize().unwrap()
    }
}

/// What one call is given.
struct Call {
    folder: PathBuf,
    prompt: Option<String>,
    globs: Option<Vec<String>>,
    edit: bool,
    cap_usd: Option<f64>,
    max_output: Option<u32>,
    max_turns: usize,
}

impl Call {
    fn new(folder: &Path) -> Self {
        Self {
            folder: folder.to_path_buf(),
            prompt: None,
            globs: None,
            edit: false,
            cap_usd: None,
            max_output: None,
            max_turns: 20,
        }
    }
    fn prompt(mut self, prompt: &str) -> Self {
        self.prompt = Some(prompt.to_string());
        self
    }
    fn globs(mut self, globs: &[&str]) -> Self {
        self.globs = Some(globs.iter().map(|g| (*g).to_string()).collect());
        self
    }
    fn edit(mut self) -> Self {
        self.edit = true;
        self
    }
    fn cap(mut self, usd: f64) -> Self {
        self.cap_usd = Some(usd);
        self
    }
    fn max_output(mut self, tokens: u32) -> Self {
        self.max_output = Some(tokens);
        self
    }
}

/// A session that has been created and has its task queued.
struct Started {
    id: SessionId,
    meter: TokenMeter,
    io: Arc<ExecIOProvider>,
    drain: tokio::task::JoinHandle<()>,
}

/// Create one session the way a program that runs many calls does: its own
/// config, its own input channel carrying the task, its own meter, and then its
/// own permission posture.
async fn start(rig: &Rig, call: &Call) -> Started {
    let (io, input, mut output) = ExecIOProvider::new(256);
    let io = Arc::new(io);
    input
        .send(IOMessage::from_external(
            "test",
            IOContent::Text("Do the task.".to_string()),
        ))
        .await
        .unwrap();
    drop(input);
    let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });

    let meter = TokenMeter::new().with_pricing_store(PriceStore::builtin());
    if let Some(cap) = call.cap_usd {
        meter.set_task_cost_cap(cap).unwrap();
    }
    let mut config = AgentConfig::new(MODEL_ID, call.folder.clone());
    if let Some(prompt) = &call.prompt {
        config = config.with_system_prompt(prompt.clone());
    }
    if let Some(globs) = &call.globs {
        config = config.with_command_globs(globs.clone());
    }
    if let Some(tokens) = call.max_output {
        config = config.with_max_output_tokens(std::num::NonZeroU32::new(tokens).unwrap());
    }
    let user_io = UserIO::new(io.clone());
    let init_meter = meter.clone();
    let id = SessionId::new();
    rig.sessions
        .create_session_with_executor(
            rig.sessions.create_context(),
            &id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            polaris::graph::GraphExecutor::new().with_default_max_iterations(call.max_turns),
            move |ctx| {
                ctx.insert(config);
                ctx.insert(user_io);
                ctx.insert(init_meter);
            },
        )
        .expect("the session is created");

    let mut policy = PolicySnapshot::defaults();
    policy.permission = PermissionMode::Unattended(UnattendedGrants {
        edit_file: call.edit,
        run_command_patterns: call.globs.clone().unwrap_or_else(|| vec!["*".to_string()]),
    })
    .permission_policy();
    rig.sessions
        .with_context(&id, move |ctx| {
            ctx.insert(policy);
        })
        .await
        .expect("the posture is installed");
    Started {
        id,
        meter,
        io,
        drain,
    }
}

impl Started {
    /// Run the turn and delete the session.
    async fn drive(self, rig: &Rig) -> (TurnOutcome, TokenMeter) {
        let outcome = run::drive_turn(&rig.sessions, &self.id).await;
        self.io.close().await;
        let _ = self.drain.await;
        rig.sessions.delete_session(&self.id).await.unwrap();
        (outcome, self.meter)
    }
}

async fn run_call(rig: &Rig, call: &Call) -> (TurnOutcome, TokenMeter) {
    start(rig, call).await.drive(rig).await
}

fn failure_message(outcome: &TurnOutcome) -> String {
    match &outcome.reason {
        TurnReason::Failed(failure) => failure.message.clone(),
        other => panic!("the turn was expected to fail, not {other:?}"),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Roots
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_on_one_server_each_use_only_their_own_folder() {
    let rig = rig().await;
    let alpha = rig.folder("alpha");
    let beta = rig.folder("beta");
    std::fs::write(alpha.join("alpha-only.txt"), "alpha-secret-contents\n").unwrap();
    std::fs::write(beta.join("beta-only.txt"), "beta-secret-contents\n").unwrap();
    let key = |folder: &Path| format!("Working directory: {}", folder.display());
    rig.provider.script(
        &key(&alpha),
        vec![
            tool("list_files", json!({ "path": "." })),
            tool("read_file", json!({ "path": "alpha-only.txt" })),
            tool("search", json!({ "pattern": "secret", "literal": true })),
            tool(
                "create_file",
                json!({ "path": "made.txt", "content": "from alpha\n" }),
            ),
            tool("run_command", json!({ "command": "pwd" })),
            text("alpha done"),
        ],
    );
    rig.provider.script(
        &key(&beta),
        vec![
            tool("list_files", json!({ "path": "." })),
            tool("read_file", json!({ "path": "beta-only.txt" })),
            tool("search", json!({ "pattern": "secret", "literal": true })),
            tool(
                "create_file",
                json!({ "path": "made.txt", "content": "from beta\n" }),
            ),
            tool("run_command", json!({ "command": "pwd" })),
            text("beta done"),
        ],
    );
    let alpha_call = Call::new(&alpha).edit().globs(&["pwd"]);
    let beta_call = Call::new(&beta).edit().globs(&["pwd"]);
    // Both sessions exist at the same time; each one runs and is deleted.
    let alpha_session = start(&rig, &alpha_call).await;
    let beta_session = start(&rig, &beta_call).await;
    let ((alpha_out, _), (beta_out, _)) =
        tokio::join!(alpha_session.drive(&rig), beta_session.drive(&rig));
    assert!(matches!(alpha_out.reason, TurnReason::Completed));
    assert!(matches!(beta_out.reason, TurnReason::Completed));

    let alpha_seen = rig.provider.requests_for(&key(&alpha));
    let beta_seen = rig.provider.requests_for(&key(&beta));
    let alpha_last = &alpha_seen.last().expect("alpha made requests").messages;
    let beta_last = &beta_seen.last().expect("beta made requests").messages;

    // What each one read, listed and searched is its own.
    assert!(alpha_last.contains("alpha-only.txt") && alpha_last.contains("alpha-secret-contents"));
    assert!(beta_last.contains("beta-only.txt") && beta_last.contains("beta-secret-contents"));
    assert!(!alpha_last.contains("beta-only") && !alpha_last.contains("beta-secret"));
    assert!(!beta_last.contains("alpha-only") && !beta_last.contains("alpha-secret"));
    // Neither saw the folder the server's toolset was built on.
    assert!(!alpha_last.contains("SERVER-ROOT-MARKER"));
    assert!(!beta_last.contains("SERVER-ROOT-MARKER"));

    // What each one wrote landed in its own folder.
    assert_eq!(
        std::fs::read_to_string(alpha.join("made.txt")).unwrap(),
        "from alpha\n"
    );
    assert_eq!(
        std::fs::read_to_string(beta.join("made.txt")).unwrap(),
        "from beta\n"
    );
    assert!(!rig.server_root.path().join("made.txt").exists());

    // `pwd` in the shell is the call's folder.
    assert!(alpha_last.contains(&alpha.display().to_string()));
    assert!(beta_last.contains(&beta.display().to_string()));
    assert!(!alpha_last.contains(&beta.display().to_string()));
    assert!(!beta_last.contains(&alpha.display().to_string()));

    // Each call billed its own meter; the server's stayed at zero.
    assert_eq!(rig.global_meter.total_tokens(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_path_that_leaves_the_folder_is_refused() {
    let rig = rig().await;
    let folder = rig.folder("call");
    let outside = rig.folder("outside");
    std::fs::write(outside.join("secret.txt"), "TOP-SECRET-VALUE\n").unwrap();
    std::fs::write(rig.parent.path().join("sibling.txt"), "SIBLING-VALUE\n").unwrap();
    std::os::unix::fs::symlink(&outside, folder.join("link")).unwrap();
    let absolute = outside.join("secret.txt");
    rig.provider.script(
        "",
        vec![
            tool("read_file", json!({ "path": "../sibling.txt" })),
            tool(
                "read_file",
                json!({ "path": absolute.display().to_string() }),
            ),
            tool("read_file", json!({ "path": "link/secret.txt" })),
            tool("list_files", json!({ "path": ".." })),
            tool(
                "create_file",
                json!({ "path": "../escaped.txt", "content": "x" }),
            ),
            tool(
                "create_file",
                json!({ "path": "link/escaped.txt", "content": "x" }),
            ),
            tool("search", json!({ "pattern": "SECRET", "path": "link" })),
            text("done"),
        ],
    );
    let (outcome, _) = run_call(&rig, &Call::new(&folder).edit()).await;
    assert!(matches!(outcome.reason, TurnReason::Completed));

    let last = rig.provider.requests().pop().unwrap().messages;
    assert_eq!(
        last.matches("escapes sandbox").count(),
        7,
        "every one of the seven attempts is refused: {last}"
    );
    assert!(!last.contains("TOP-SECRET-VALUE"));
    assert!(!last.contains("SIBLING-VALUE"));
    assert!(!rig.parent.path().join("escaped.txt").exists());
    assert!(!outside.join("escaped.txt").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shell_starts_in_the_folder_and_refuses_a_working_dir_outside_it() {
    let rig = rig().await;
    let folder = rig.folder("call");
    std::fs::create_dir(folder.join("sub")).unwrap();
    let outside = rig.folder("outside");
    rig.provider.script(
        "",
        vec![
            tool("run_command", json!({ "command": "pwd" })),
            tool(
                "run_command",
                json!({ "command": "pwd", "working_dir": "sub" }),
            ),
            tool(
                "run_command",
                json!({ "command": "pwd", "working_dir": ".." }),
            ),
            tool(
                "run_command",
                json!({ "command": "pwd", "working_dir": outside.display().to_string() }),
            ),
            text("done"),
        ],
    );
    let (outcome, _) = run_call(&rig, &Call::new(&folder).globs(&["pwd"])).await;
    assert!(matches!(outcome.reason, TurnReason::Completed));

    let last = rig.provider.requests().pop().unwrap().messages;
    assert!(
        last.contains(&format!("{}\\n", folder.display())),
        "pwd without a working_dir is the call's folder: {last}"
    );
    assert!(
        last.contains(&format!("{}/sub", folder.display())),
        "a working_dir inside the folder is honoured: {last}"
    );
    // The session's permission policy checks the directory first; the shell
    // wrapper's own check is exercised directly in the toolset test below.
    assert_eq!(
        last.matches("shell directory is invalid or outside the workspace")
            .count(),
        2,
        "both a parent directory and an absolute directory elsewhere are refused: {last}"
    );
    assert!(
        !last.contains(&format!("{}\\n", outside.display())),
        "nothing ran in the other folder"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_call_root_the_toolsets_own_root_applies() {
    let server_root = tempfile::tempdir().unwrap();
    std::fs::write(server_root.path().join("server-file.txt"), "s").unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("other-file.txt"), "o").unwrap();

    let tools = CodeTools::new(CodeToolsConfig::new(server_root.path())).tools();
    let find = |name: &str| {
        tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .unwrap_or_else(|| panic!("the toolset has a {name} tool"))
    };
    let list = find("list_files");
    let own = list
        .execute(json!({ "path": "." }), &ToolContext::new())
        .await
        .unwrap()
        .to_string();
    assert!(own.contains("server-file.txt") && !own.contains("other-file.txt"));
    let rooted_context = ToolContext::new().with(CallRoot::new(other.path()));
    let rooted = list
        .execute(json!({ "path": "." }), &rooted_context)
        .await
        .unwrap()
        .to_string();
    assert!(rooted.contains("other-file.txt") && !rooted.contains("server-file.txt"));
    assert!(
        find("read_file")
            .execute(json!({ "path": "server-file.txt" }), &rooted_context)
            .await
            .is_err(),
        "a file of the toolset's root is not reachable from another root"
    );

    let executor = ShellExecutor::new(
        ShellConfig::new()
            .with_working_dir(server_root.path())
            .with_allowed_dirs(vec![
                server_root.path().to_path_buf(),
                other.path().to_path_buf(),
            ])
            .with_allowed_commands(vec!["*".to_string()])
            .with_env_allowlist(vec!["PATH".to_string()])
            .disable_cache(),
    );
    let shell = ApprovalShellTool::new(executor.clone());
    let stdout = |value: serde_json::Value| {
        value["stdout"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let args = json!({ "command": "pwd" });
    let plain = shell
        .execute(args.clone(), &ToolContext::new())
        .await
        .unwrap();
    assert_eq!(
        stdout(plain),
        server_root
            .path()
            .canonicalize()
            .unwrap()
            .display()
            .to_string(),
        "with no CallRoot the executor's own working directory applies"
    );
    let context = ToolContext::new().with(CallRoot::new(other.path()));
    let rooted = shell.execute(args, &context).await.unwrap();
    assert_eq!(
        stdout(rooted),
        other.path().canonicalize().unwrap().display().to_string()
    );
    for outside in ["..", &server_root.path().display().to_string()] {
        let refusal = shell
            .execute(
                json!({ "command": "pwd", "working_dir": outside }),
                &context,
            )
            .await
            .expect_err("a working_dir outside the call's folder is refused")
            .to_string();
        assert!(
            refusal.contains("outside this call's folder"),
            "the wrapper names its own reason: {refusal}"
        );
    }
    executor.stop_all().await;
}

// ─────────────────────────────────────────────────────────────────────────────
// Command patterns
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_outside_the_calls_patterns_is_refused_and_one_inside_runs() {
    let rig = rig().await;
    let folder = rig.folder("call");
    rig.provider.script(
        "",
        vec![
            tool(
                "run_command",
                json!({ "command": "echo ran >> inside.txt" }),
            ),
            tool("run_command", json!({ "command": "touch outside.txt" })),
            tool(
                "run_command",
                json!({ "command": "echo ok; touch chained.txt" }),
            ),
            tool(
                "run_command",
                json!({ "command": "echo $(touch substituted.txt)" }),
            ),
            text("done"),
        ],
    );
    let (outcome, _) = run_call(&rig, &Call::new(&folder).globs(&["echo *"])).await;
    assert!(matches!(outcome.reason, TurnReason::Completed));

    assert!(folder.join("inside.txt").exists(), "a matching command ran");
    for refused in ["outside.txt", "chained.txt", "substituted.txt"] {
        assert!(!folder.join(refused).exists(), "{refused} must not exist");
    }
    let last = rig.provider.requests().pop().unwrap().messages;
    assert_eq!(
        last.matches("outside this session's command patterns")
            .count(),
        3,
        "another program, a chain that ends in one and a substitution that hides one \
         are all refused: {last}"
    );
    assert_eq!(
        last.matches("these patterns: `echo *`").count(),
        3,
        "each refusal lists the patterns the session accepts: {last}"
    );
    let first = rig.provider.requests().remove(0);
    assert!(
        first.system.contains("these patterns: `echo *`"),
        "the system prompt lists the patterns before the first command: {}",
        first.system
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn literal_scripts_reach_the_executor_only_after_all_commands_pass() {
    let rig = rig().await;
    let folder = rig.folder("scripts");
    let commands = [
        "cat >literal.txt <<'EOF'\n$(touch hidden.txt)\nEOF\ncat literal.txt",
        "python3 - <<'PY'\nfrom pathlib import Path\nPath('heredoc.txt').write_text('ok')\nPY",
        "python3 -c \"from pathlib import Path\nPath('multiline.txt').write_text('ok')\"",
        "cat literal.txt | head -1",
        "cat >refused.txt <<EOF\n$(touch expanded.txt)\nEOF",
        "cat literal.txt <<'EOF'\ndata\nEOF\ntouch after.txt",
        "cat literal.txt\nrm -rf literal.txt",
    ];
    rig.provider.script(
        "",
        vec![
            tool("run_command", json!({"command":commands[0]})),
            tool("run_command", json!({"command":commands[1]})),
            tool("run_command", json!({"command":commands[2]})),
            tool("run_command", json!({"command":commands[3]})),
            tool("run_command", json!({"command":commands[4]})),
            tool("run_command", json!({"command":commands[5]})),
            tool("run_command", json!({"command":commands[6]})),
            text("done"),
        ],
    );
    let (outcome, _) = run_call(
        &rig,
        &Call::new(&folder).globs(&["cat *", "python3 *", "head *", "rm *"]),
    )
    .await;
    assert!(matches!(outcome.reason, TurnReason::Completed));
    assert_eq!(
        std::fs::read_to_string(folder.join("literal.txt")).unwrap(),
        "$(touch hidden.txt)\n"
    );
    for name in ["heredoc.txt", "multiline.txt"] {
        assert_eq!(std::fs::read_to_string(folder.join(name)).unwrap(), "ok");
    }
    for name in ["hidden.txt", "refused.txt", "expanded.txt", "after.txt"] {
        assert!(!folder.join(name).exists(), "{name}");
    }
    let requests = rig.provider.requests();
    let messages = &requests.last().unwrap().messages;
    assert!(!messages.contains("confirmation_required"), "{messages}");
    assert!(messages.contains("shell's deny list"), "{messages}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_with_no_patterns_has_no_shell_tool_in_the_request() {
    let rig = rig().await;
    let none = rig.folder("no-shell");
    let some = rig.folder("with-shell");
    rig.provider
        .script(&none.display().to_string(), vec![text("done")]);
    rig.provider
        .script(&some.display().to_string(), vec![text("done")]);
    run_call(&rig, &Call::new(&none).globs(&[])).await;
    run_call(&rig, &Call::new(&some).globs(&["pwd"])).await;

    let without = &rig.provider.requests_for(&none.display().to_string())[0].tools;
    let with = &rig.provider.requests_for(&some.display().to_string())[0].tools;
    for shell_tool in ["run_command", "run_in_background", "stop_task"] {
        assert!(
            !without.iter().any(|name| name == shell_tool),
            "{shell_tool} must not be offered to a call with no command patterns: {without:?}"
        );
    }
    assert!(without.iter().any(|name| name == "read_file"));
    assert!(with.iter().any(|name| name == "run_command"));
}

// ─────────────────────────────────────────────────────────────────────────────
// Role prompt
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_call_sends_its_own_role_prompt_and_keeps_the_grounding() {
    let rig = rig().await;
    let one = rig.folder("one");
    let two = rig.folder("two");
    rig.provider
        .script("ROLE-PROMPT-ONE", vec![text("one done")]);
    rig.provider
        .script("ROLE-PROMPT-TWO", vec![text("two done")]);
    run_call(
        &rig,
        &Call::new(&one).prompt("ROLE-PROMPT-ONE: be an ideator."),
    )
    .await;
    run_call(
        &rig,
        &Call::new(&two).prompt("ROLE-PROMPT-TWO: be a reviewer."),
    )
    .await;

    let first = rig.provider.requests_for("ROLE-PROMPT-ONE");
    let second = rig.provider.requests_for("ROLE-PROMPT-TWO");
    assert_eq!((first.len(), second.len()), (1, 1));
    for (seen, role, folder, other) in [
        (
            &first[0],
            "ROLE-PROMPT-ONE: be an ideator.",
            &one,
            "ROLE-PROMPT-TWO",
        ),
        (
            &second[0],
            "ROLE-PROMPT-TWO: be a reviewer.",
            &two,
            "ROLE-PROMPT-ONE",
        ),
    ] {
        assert!(
            seen.system.starts_with(role),
            "the prompt is the call's: {}",
            seen.system
        );
        assert!(
            !seen.system.contains("precise terminal coding agent"),
            "the built-in text is replaced"
        );
        assert!(
            seen.system.contains("<environment>")
                && seen
                    .system
                    .contains(&format!("Working directory: {}", folder.display())),
            "the grounding block is still appended: {}",
            seen.system
        );
        assert!(
            !seen.system.contains(other),
            "a call never sees another call's prompt"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_role_prompt_the_request_is_the_one_the_agent_always_sent() {
    let rig = rig().await;
    let folder = rig.folder("plain");
    rig.provider.script("", vec![text("done")]);
    let (outcome, _) = run_call(&rig, &Call::new(&folder)).await;
    assert!(matches!(outcome.reason, TurnReason::Completed));

    let seen = rig.provider.requests().pop().unwrap();
    assert!(seen.system.starts_with(BUILT_IN_PROMPT_START));
    let (built_in, grounding) = seen
        .system
        .split_once("\n\n<environment>\n")
        .expect("the grounding block follows the prompt");
    assert!(built_in.contains("Follow the ReAct pattern"));
    assert!(grounding.starts_with(&format!("Working directory: {}", folder.display())));
    assert!(seen.cache_prefix, "the stable prefix is still cached");
    assert_eq!(
        seen.max_output_tokens, None,
        "the request leaves the output limit to the provider"
    );
    for tool in ["read_file", "list_files", "search", "glob", "run_command"] {
        assert!(
            seen.tools.iter().any(|name| name == tool),
            "{tool} is offered"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_configured_output_allowance_limits_the_request() {
    let rig = rig().await;
    let folder = rig.folder("limited");
    rig.provider.script("", vec![text("done")]);
    run_call(&rig, &Call::new(&folder).max_output(777)).await;
    assert_eq!(rig.provider.requests()[0].max_output_tokens, Some(777));
}

// ─────────────────────────────────────────────────────────────────────────────
// Spend cap
// ─────────────────────────────────────────────────────────────────────────────

/// Replies that bill $0.04 each ($0.02 of input and $0.02 of output).
fn costly(name: &'static str, arguments: serde_json::Value) -> Reply {
    Reply::Tool {
        name,
        arguments,
        input: 10_000,
        output: 2_000,
    }
}

fn costly_text(text: &'static str) -> Reply {
    Reply::Text {
        text,
        input: 10_000,
        output: 2_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_below_any_request_ends_the_call_before_the_model_is_called() {
    let rig = rig().await;
    let folder = rig.folder("tiny-cap");
    rig.provider.script("", vec![text("never sent")]);
    let (outcome, meter) = run_call(&rig, &Call::new(&folder).cap(0.0001)).await;

    assert_eq!(
        rig.provider.requests().len(),
        0,
        "the model was never called"
    );
    let message = failure_message(&outcome);
    assert!(message.starts_with("cost-cap:"), "{message}");
    assert!(meter.cost_cap_stopped());
    assert_eq!(meter.total_tokens(), 0);
    assert_eq!(outcome.usage.as_ref().map(|usage| usage.num_turns), Some(0));
    assert_eq!(rig.global_meter.total_tokens(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_that_admits_one_request_but_not_the_next_ends_the_call_after_that_one() {
    let rig = rig().await;
    let folder = rig.folder("one-request");
    rig.provider.script(
        "",
        vec![
            costly("list_files", json!({ "path": "." })),
            costly("list_files", json!({ "path": "." })),
            costly_text("never sent"),
        ],
    );
    // One request reserves $0.04 of output plus its input; it bills $0.04. The
    // second would reserve the same against the $0.04 that is left.
    let (outcome, meter) = run_call(&rig, &Call::new(&folder).cap(0.08).max_output(4_000)).await;

    assert_eq!(
        rig.provider.requests().len(),
        1,
        "exactly the admitted request"
    );
    let message = failure_message(&outcome);
    assert!(message.starts_with("cost-cap:"), "{message}");
    assert!(meter.cost_cap_stopped());
    assert_eq!(meter.input_tokens(), 10_000);
    assert_eq!(meter.output_tokens(), 2_000);
    let usage = outcome.usage.expect("the call was measured");
    assert_eq!(usage.num_turns, 1);
    assert_eq!((usage.input_tokens, usage.output_tokens), (10_000, 2_000));
    assert!(
        (usage.estimated_cost_usd.unwrap() - 0.04).abs() < 1e-9,
        "{:?}",
        usage.estimated_cost_usd
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_request_is_not_retried() {
    let rig = rig().await;
    let folder = rig.folder("not-retried");
    rig.provider.script("", vec![costly_text("never sent")]);
    // Refused on the first attempt; the retry policy would otherwise try again.
    let (outcome, _) = run_call(&rig, &Call::new(&folder).cap(0.01).max_output(4_000)).await;
    assert!(failure_message(&outcome).starts_with("cost-cap:"));
    assert_eq!(rig.provider.requests().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cap_that_admits_every_request_changes_nothing_and_the_meter_matches_the_bills() {
    let rig = rig().await;
    let script = || {
        vec![
            costly("list_files", json!({ "path": "." })),
            costly("list_files", json!({ "path": "." })),
            costly_text("finished"),
        ]
    };
    let capped = rig.folder("capped");
    let free = rig.folder("uncapped");
    rig.provider.script(&capped.display().to_string(), script());
    rig.provider.script(&free.display().to_string(), script());
    let (capped_out, capped_meter) =
        run_call(&rig, &Call::new(&capped).cap(5.0).max_output(4_000)).await;
    let (free_out, free_meter) = run_call(&rig, &Call::new(&free).max_output(4_000)).await;

    for (outcome, meter) in [(&capped_out, &capped_meter), (&free_out, &free_meter)] {
        assert!(matches!(outcome.reason, TurnReason::Completed));
        assert_eq!(outcome.final_text.as_deref(), Some("finished"));
        assert_eq!(
            meter.input_tokens(),
            30_000,
            "three replies of 10 000 billed"
        );
        assert_eq!(meter.output_tokens(), 6_000);
        assert!(!meter.cost_cap_stopped());
    }
    // The same script, with and without a cap, reads the same conversation.
    let strip = |folder: &Path| {
        rig.provider
            .requests_for(&folder.display().to_string())
            .into_iter()
            .map(|seen| (seen.tools, seen.max_output_tokens, seen.cache_prefix))
            .collect::<Vec<_>>()
    };
    assert_eq!(strip(&capped), strip(&free));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_refused_by_the_provider_gives_its_reservation_back() {
    let rig = rig().await;
    let folder = rig.folder("refused");
    rig.provider.script("", vec![Reply::Refuse]);
    let (outcome, meter) = run_call(&rig, &Call::new(&folder).cap(0.065).max_output(4_000)).await;
    assert!(matches!(outcome.reason, TurnReason::Failed(_)));
    assert!(
        probe_fits(&meter, 3_000),
        "nothing was billed, so the whole cap is still there"
    );
}

/// Whether one more small request still fits under the meter's cap. A probe
/// that reserves and is dropped leaves the meter as it found it.
fn probe_fits(meter: &TokenMeter, max_output: u32) -> bool {
    let request = LlmRequest {
        messages: vec![Message::user("probe")],
        ..Default::default()
    };
    meter
        .reserve_request(MODEL_ID, &request, max_output)
        .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_call_dropped_with_its_request_in_flight_is_charged_at_the_estimate() {
    let rig = rig().await;
    let folder = rig.folder("dropped");
    let entered = Arc::new(Notify::new());
    rig.provider.script(
        "",
        vec![Reply::Stall {
            entered: entered.clone(),
        }],
    );
    let call = Call::new(&folder).cap(0.065).max_output(4_000);
    let started = start(&rig, &call).await;
    let meter = started.meter.clone();
    assert!(
        probe_fits(&meter, 3_000),
        "before the call, the cap has room"
    );

    let sessions = rig.sessions.clone();
    let id = started.id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&sessions, &id).await });
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("the request reached the model");
    turn.abort();
    let _ = turn.await;
    started.io.close().await;
    rig.sessions.delete_session(&started.id).await.unwrap();

    assert!(
        !probe_fits(&meter, 3_000),
        "the dropped request's estimate was charged, not released"
    );
    assert_eq!(meter.total_tokens(), 0, "no usage was ever reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_interrupted_call_is_charged_at_the_estimate() {
    let rig = rig().await;
    let folder = rig.folder("interrupted");
    let entered = Arc::new(Notify::new());
    rig.provider.script(
        "",
        vec![Reply::Stall {
            entered: entered.clone(),
        }],
    );
    let call = Call::new(&folder).cap(0.065).max_output(4_000);
    let started = start(&rig, &call).await;
    let meter = started.meter.clone();

    let sessions = rig.sessions.clone();
    let id = started.id.clone();
    let turn = tokio::spawn(async move { run::drive_turn(&sessions, &id).await });
    tokio::time::timeout(Duration::from_secs(20), entered.notified())
        .await
        .expect("the request reached the model");
    assert!(run::request_interrupt(&rig.sessions, &started.id));
    let outcome = turn.await.unwrap();
    started.io.close().await;
    rig.sessions.delete_session(&started.id).await.unwrap();

    assert!(matches!(outcome.reason, TurnReason::Interrupted));
    assert!(!probe_fits(&meter, 3_000));
}
