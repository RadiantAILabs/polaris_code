//! A complete embedder: a program that is not `polaris-code`, driving one
//! coding turn through `polaris_code_core`'s public API.
//!
//! This is the runnable form of the guide in
//! [`polaris_code_core::docs::embedding`] (`src/docs/embedding.md`). Nothing
//! here reaches into the binary's composition root — it builds its own, the
//! way any embedder has to.
//!
//! ```bash
//! ANTHROPIC_API_KEY=… cargo run -p polaris_code_core --example embed_turn -- . "what does README.md say?"
//! ```
//!
//! Exit codes are the run surface's own
//! ([`TurnReason::exit_code`](polaris_code_core::run::TurnReason::exit_code)):
//! `0` the turn completed, `1` it failed, `130` it was interrupted, and `2` for
//! a usage or configuration error before any turn started. Ctrl-C asks the
//! turn to stop through
//! [`run::request_interrupt`](polaris_code_core::run::request_interrupt).
//!
//! As its own composition root, this example prints — the no-`println!` rule
//! binds the library, not a host. A richer front end would render the drained
//! `IOMessage`s instead of discarding them; see
//! [`exec::stream_json_line`](polaris_code_core::exec::stream_json_line).

use std::path::PathBuf;
use std::sync::Arc;

use polaris::models::anthropic::DEFAULT_MAX_TOKENS;
use polaris::models::{AnthropicPlugin, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code_core::exec::{ExecIOPlugin, ExecIOProvider, OutputFormat, RunReport};
use polaris_code_core::run;
use polaris_code_core::tools::{CodeToolsConfig, CodeToolsPlugin};
use polaris_code_core::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;

/// The environment variable `AnthropicPlugin` reads the model key from.
const API_KEY_VAR: &str = "ANTHROPIC_API_KEY";

/// The model this embedder configures its session with.
const MODEL_ID: &str = "anthropic/claude-sonnet-5";

/// Output-channel capacity for the channel-backed `UserIO`.
const OUTPUT_BUFFER: usize = 64;

/// Exit code for a usage or configuration error, before any turn ran.
const EXIT_CONFIG_ERROR: i32 = 2;

/// Report a start-up failure and end the process, exactly as a host owes its
/// caller: one line, and a code that is not the turn's.
fn fatal(message: &str) -> ! {
    eprintln!("embed_turn: {message}");
    std::process::exit(EXIT_CONFIG_ERROR);
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let working_dir = args.next().map_or_else(
        || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        PathBuf::from,
    );
    let Some(prompt) = args.next() else {
        fatal("usage: embed_turn <working_dir> \"<prompt>\"");
    };
    let working_dir = working_dir
        .canonicalize()
        .unwrap_or_else(|err| fatal(&format!("invalid working directory: {err}")));
    if std::env::var(API_KEY_VAR).is_err() {
        fatal(&format!(
            "{API_KEY_VAR} is not set; an embedder still needs a model key"
        ));
    }

    // 1. The front end: one channel-backed `UserIO` instead of the TUI panels.
    let (io, input, mut output) = ExecIOProvider::new(OUTPUT_BUFFER);
    let io = Arc::new(io);

    // 2. The server. The meter is a global, inserted before any session
    //    context exists, or every turn would report `usage: None`.
    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(AnthropicPlugin::from_env(API_KEY_VAR))
        .add_plugins(ToolsPlugin)
        .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(&working_dir)))
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            working_dir.join(".polaris/sessions"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(io.clone()));
    if let Err(err) = server.finish().await {
        fatal(&format!("failed to start the server: {err}"));
    }

    // 3. Sessions: serializers, the agent, and one session carrying its config.
    let Some(persistence) = server.api::<PersistenceAPI>() else {
        fatal("PersistenceAPI missing");
    };
    let Some(sessions) = server.api::<SessionsAPI>().cloned() else {
        fatal("SessionsAPI missing");
    };
    sessions.set_serializers(persistence.serializers());
    if let Err(err) = sessions.register_agent(CodeAgent) {
        fatal(&format!("failed to register CodeAgent: {err}"));
    }

    let session_id = SessionId::new();
    let config = AgentConfig::new(MODEL_ID, working_dir.clone());
    if let Err(err) = sessions.create_session_with(
        server.create_context(),
        &session_id,
        &AgentTypeId::from_name(CodeAgent::NAME),
        move |ctx| {
            ctx.insert(config.clone());
        },
    ) {
        fatal(&format!("failed to create session: {err}"));
    }

    // 4. The posture, after creation — `CodeAgent::setup` has already seeded
    //    `PolicySnapshot::defaults()`, whose permission policy is empty. This
    //    embedder has no human to ask, so it grants nothing: read-only tools
    //    run, `edit_file` and `run_command` are refused rather than parked on
    //    a confirmation nobody would answer.
    let mut policy = PolicySnapshot::defaults();
    policy.permission = PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
    if let Err(err) = sessions
        .with_context(&session_id, move |ctx| {
            ctx.insert(policy);
        })
        .await
    {
        fatal(&format!("failed to apply the permission policy: {err}"));
    }

    // 5. Ctrl-C asks the turn to stop. The signal handler needs nothing but a
    //    cloned API and id — cancellation is not private to the binary.
    let signal_sessions = sessions.clone();
    let signal_id = session_id.clone();
    let signals = tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if run::request_interrupt(&signal_sessions, &signal_id) {
                eprintln!("embed_turn: interrupting the running turn…");
            }
        }
    });

    // 6. Enqueue the prompt before the turn starts — `receive_user_input`
    //    swallows a closed channel rather than erroring, so a send racing the
    //    turn could hang — and drain concurrently, because the output channel
    //    is bounded and would otherwise backpressure the agent.
    if let Err(err) = input
        .send(IOMessage::from_external(
            "embedder",
            IOContent::Text(prompt),
        ))
        .await
    {
        fatal(&format!("failed to enqueue the prompt: {err}"));
    }
    drop(input);
    let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });

    let outcome = run::drive_turn(&sessions, &session_id).await;

    io.close().await;
    let _ = drain.await;
    signals.abort();

    // 7. Report. `RunReport` is the binary's own renderer, reused here rather
    //    than reimplemented: identity on stderr, the answer on stdout.
    if let Err(err) = sessions.save_session(&session_id).await {
        eprintln!("embed_turn: failed to save the session: {err}");
    }
    let report = RunReport::new(&session_id, &outcome, DEFAULT_MAX_TOKENS.get());
    eprintln!("{}", report.identity_line());
    if let Some(line) = report.error_line() {
        eprintln!("{line}");
    }
    if let Some(payload) = report.stdout_payload(OutputFormat::Text) {
        println!("{payload}");
    }
    if let Some(usage) = &outcome.usage {
        eprintln!(
            "embed_turn: {} tokens, estimated {}",
            usage.total_tokens,
            usage
                .estimated_cost_usd
                .map_or_else(|| "unpriced".to_string(), |cost| format!("${cost:.4}"))
        );
    }

    std::process::exit(report.exit_code());
}
