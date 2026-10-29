# Embedding the coding agent in another program

This is the starting point for a program that is **not** the `polaris-code`
binary and wants to drive a coding turn itself: an evaluation runner, a test
harness, a protocol server, another front end.

There is no `Harness` type, no builder and no `init()`. Embedding is three
things in order:

1. compose the plugins you want on a [`Server`](polaris::system::server::Server);
2. create a session that carries an [`AgentConfig`](crate::agents::AgentConfig);
3. call [`run::drive_turn`](crate::run::drive_turn).

The run surface is three free functions — [`drive_turn`](crate::run::drive_turn),
[`drive_until`](crate::run::drive_until) and
[`request_interrupt`](crate::run::request_interrupt) — and they hold no state.
The composition root is yours, exactly as `src/main.rs` is the binary's.

`polaris_code` exposes the agent as a library package. Add it from crates.io
and compose the plugins your application needs; the dependency list below
matches the minimal embedder.

## Dependencies

Add these direct dependencies to the application that runs the full example:

```toml
[dependencies]
polaris_code = "0.1.0"
polaris = { package = "polaris-ai", version = "0.7.0", default-features = false, features = ["anthropic"] }
polaris_ontrace = { version = "0.1.0", default-features = false, features = ["oxigraph"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

`polaris` is the Rust import name for the `polaris-ai` package. Keep its version
compatible with the runtime used by `polaris_code` so their shared framework
types resolve to one package instance.

## What you must supply

| You supply | Why |
|------------|-----|
| **Exactly one `UserIO` front end.** [`exec::ExecIOPlugin`](crate::exec::ExecIOPlugin) over [`exec::ExecIOProvider`](crate::exec::ExecIOProvider) is the channel-backed one; `polaris-tui`'s conversation panel is the other. | The agent takes its prompt and publishes its answer, its tool blocks and its permission questions through `UserIO`. Install one or the other — **never both**; the second registration wins and the first front end goes silent. |
| **A model provider.** `ModelsPlugin`, plus a plugin that registers an `LlmProvider` (`AnthropicPlugin::from_env("ANTHROPIC_API_KEY")`, or your own). | `CodeAgent::setup` resolves the model by id and fails the session if it cannot. [`AgentConfig::model_id`](crate::agents::AgentConfig) must be `<provider name>/<model>` — the prefix is matched against `LlmProvider::name()`. |
| **`ToolsPlugin` plus whichever capability plugins you want.** [`CodeToolsPlugin`](crate::tools::CodeToolsPlugin), [`CodeGraphPlugin`](crate::tools::CodeGraphPlugin), [`MemoryToolsPlugin`](crate::tools::MemoryToolsPlugin), `ShellPlugin`. | These are the agent's hands. All of them are optional: with none installed the turn still runs, the model simply has nothing to call. Each is sandboxed to the `working_dir` you give it, and that directory should be the one in your `AgentConfig`. |
| **`PersistencePlugin`, `SessionsPlugin`, [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin), and ontrace's `OxigraphPlugin`.** | Sessions and their serializers, the agent graph itself, and the RDF store the agent reifies its decisions into. `OxigraphPlugin::in_memory()` is enough; the agent writes its trajectory there whether or not you read it. |
| **A [`TokenMeter`](crate::usage::TokenMeter) inserted as a global _before_ the first `create_context()`.** | Not optional, and the failure is quiet: the agent's `act` system takes the meter as a required `Res<TokenMeter>`, so a session context without one cannot run that step at all — the turn comes back `Completed` with `final_text: None` and `usage: None`, as if the model had said nothing. [`TurnOutcome::usage`](crate::run::TurnOutcome) reads the same meter back, and `usage: None` always means **unmeasured, not free**. |
| **A session carrying an `AgentConfig`, and then a [`PolicySnapshot`](crate::agents::PolicySnapshot).** | The config names the model and the working directory. The policy is what every control decision reads — see the warning below, it is not optional in practice. |

## What you get back

[`run::drive_turn`](crate::run::drive_turn) never panics and never returns an
error; a session-level failure comes back as
[`TurnReason::Failed`](crate::run::TurnReason::Failed). It returns one
[`TurnOutcome`](crate::run::TurnOutcome):

| Field | What an embedder does with it |
|-------|-------------------------------|
| `reason` | `Completed`, `Failed(message)`, `Interrupted`, or `NoTopologyStarted`. [`TurnReason::exit_code`](crate::run::TurnReason::exit_code) maps it to the CLI contract (`0`/`1`/`130`) if you are exposing one. [`run::driver_continues`](crate::run::driver_continues) says whether a looping driver should start another turn. |
| `final_text` | The turn's answer, read out of the canonical history rather than out of your IO stream — so it does not matter which provider you installed. `None` for every non-`Completed` outcome. |
| `usage` | The run's accounting as [`RunUsage`](crate::usage::RunUsage), schema `polaris-code-run-usage/v1`. Reported for **every** outcome, including an interrupted one. `None` means unmeasured, never free. `docs/headless-usage-accounting.md` specifies the shape field by field; it is a published external contract and this is the object it describes. |
| `run_id`, `nodes_executed`, `duration` | The trace join key into graph events, and what the turn actually did. `run_id` is `None` when the turn never reached the executor. |
| `turn_number` | The session's own count, read back after the turn. It counts turns that **started**: an interrupted turn advances it, and the next prompt is a new turn rather than a retry. |
| `topology` | `None` for an ordinary conversational turn. `Some` only if you accepted a coding task into `cosmos::host::CodingTaskQueue`; see `docs/topology-selection.md`. |

Everything the turn published on the way — assistant text deltas, tool blocks,
permission questions, live token deltas — arrives as `IOMessage`s on the
receiver [`ExecIOProvider::new`](crate::exec::ExecIOProvider::new) handed you.
Drain it concurrently with the turn: the channel is bounded, and a front end
that stops draining backpressures the agent.

## Interrupting a turn

[`run::request_interrupt`](crate::run::request_interrupt) raises the session's
cancellation signal from outside the turn. It returns `true` if a turn was
actually in flight, and `false` for an idle or unknown session — it is not an
error to ask when there is nothing to stop.

The interrupted turn ends with `TurnReason::Interrupted`, the session stays
`Active`, a checkpoint is left behind, and the next prompt continues normally.
If you want the two lifecycle messages (`io_type = "interrupting"`, then
`"cancelled"`) on your own IO stream, drive the turn with
[`drive_turn_with_status`](crate::run::drive_turn_with_status) and a
[`StatusEmitter`](crate::run::StatusEmitter) built over your provider; plain
`drive_turn` publishes nothing.

## What an embedder cannot do

A documented entry point that hides a constraint is worse than none, so:

* **The default policy is not a safe posture.**
  [`PolicySnapshot::defaults()`](crate::agents::PolicySnapshot::defaults) — which
  `CodeAgent::setup` seeds for you — carries an *empty*
  [`PermissionPolicy`](crate::agents::PermissionPolicy), so every tool falls back
  to its registry default. That means `edit_file` asks, and `run_command`
  **runs unprompted**. Insert your own snapshot with
  [`PermissionMode::permission_policy`](crate::agents::PermissionMode::permission_policy)
  after creating the session, and use
  [`PermissionMode::Unattended`](crate::agents::PermissionMode::Unattended) if no
  human is watching. `Unattended` is the only mode that is fail-closed on an
  ambiguous shell verdict. See `docs/permission-modes.md`.
* **A confirmation parks the turn until you answer it.** `UserIO::confirm`
  sends a question with `io_type = "confirm"` and then blocks on `receive()` —
  the same channel your prompt came in on. Under an attended posture your
  program *is* the human: reply with an `IOMessage` carrying metadata
  `confirmed = "true"`, or the turn waits forever. Dropping the input sender
  ends the wait as a refusal, not as an approval.
* **One front end, chosen at composition time.** You cannot embed the TUI
  inside another UI, and you cannot run the TUI panels and `ExecIOPlugin`
  together to get "both". Pick one when you build the `Server`.
* **The CLI is not part of the library.** `--print`, `--output-format`,
  `--session`, `--allow-edit`, the exit codes and the identity line live in
  `src/main.rs`. [`exec::report`](crate::exec::report) *renders* those formats
  and is public, but nothing in this crate prints: the composition root owns
  stdout. If you want `json` output, call
  [`RunReport`](crate::exec::RunReport) and print it yourself.
* **The `POLARIS_CODE_*` environment variables are the binary's.** The model,
  the graph-search mode and the prompt-cache strategy are read from the
  environment by `main.rs` only. An embedder sets them on
  [`AgentConfig`](crate::agents::AgentConfig) (`with_graph_search`,
  `with_prompt_cache`) and on [`CodeGraphPlugin`](crate::tools::CodeGraphPlugin).
* **Terminal behaviour is front-end policy, not agent behaviour.** `/commands`,
  `@` mentions, session switching, rewind, background tasks and turn steering
  are wired by `main.rs` out of [`terminal`](crate::terminal) parts. An embedder
  gets none of them by default and must wire what it wants.
* **[`drive_until`](crate::run::drive_until) is for an interactive front end.**
  It is paced by `UserIO::receive`, so it returns only when a turn genuinely
  fails. A one-shot embedder calls `drive_turn` once, with the prompt already
  enqueued.
* **Saving is yours.** Nothing in the run surface persists the session; call
  `SessionsAPI::save_session` when you want the transcript on disk, and give
  `SessionsPlugin` a `FileStore` rather than an `InMemoryStore` if you want it
  to outlive the process.

## The minimal embedder

This is the whole thing. It compiles against public API only; the consumer that
runs it with a scripted model — and asserts the outcome, the usage, the
confirmation answer and the interrupt — is
`tests/embedding_entry_point.rs`, and the runnable version of this same program
is `examples/embed_turn.rs`.

```rust,no_run
use std::sync::Arc;

use polaris::models::{AnthropicPlugin, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode, PolicySnapshot, UnattendedGrants,
};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run::{self, TurnReason};
use polaris_code::tools::{CodeToolsConfig, CodeToolsPlugin};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;

# async fn embed(working_dir: std::path::PathBuf, prompt: String)
#     -> Result<(), Box<dyn std::error::Error>> {
// 1. The front end: one channel-backed UserIO instead of the TUI panels.
let (io, input, mut output) = ExecIOProvider::new(64);
let io = Arc::new(io);

// 2. The server. The meter goes in as a global before any context exists.
let mut server = Server::new();
server.insert_global(TokenMeter::new());
server
    .add_plugins(ModelsPlugin)
    .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
    .add_plugins(ToolsPlugin)
    .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(&working_dir)))
    .add_plugins(OxigraphPlugin::in_memory())
    .add_plugins(PersistencePlugin)
    .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
    .add_plugins(CodeAgentPlugin::new())
    .add_plugins(ExecIOPlugin::new(io.clone()));
server.finish().await?;

// 3. The session: serializers, the agent, and one session carrying its config.
let persistence = server.api::<PersistenceAPI>().expect("PersistencePlugin");
let sessions = server.api::<SessionsAPI>().expect("SessionsPlugin").clone();
sessions.set_serializers(persistence.serializers());
sessions.register_agent(CodeAgent)?;

let session_id = SessionId::new();
let config = AgentConfig::new("anthropic/claude-sonnet-5", working_dir);
sessions.create_session_with(
    server.create_context(),
    &session_id,
    &AgentTypeId::from_name(CodeAgent::NAME),
    move |ctx| {
        ctx.insert(config.clone());
    },
)?;

// 4. The posture, after creation: this one never asks and never edits.
let mut policy = PolicySnapshot::defaults();
policy.permission = PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
sessions
    .with_context(&session_id, move |ctx| {
        ctx.insert(policy);
    })
    .await?;

// 5. Enqueue the prompt *before* the turn starts, drain concurrently, run it.
input
    .send(IOMessage::from_external("embedder", IOContent::Text(prompt)))
    .await?;
let drain = tokio::spawn(async move { while output.recv().await.is_some() {} });

let outcome = run::drive_turn(&sessions, &session_id).await;

io.close().await;
drain.await?;

// 6. Read the outcome and what it cost.
match &outcome.reason {
    TurnReason::Completed => println!("{}", outcome.final_text.unwrap_or_default()),
    reason => eprintln!("turn did not complete: {reason:?}"),
}
if let Some(usage) = &outcome.usage {
    eprintln!("{} tokens, {:?} USD", usage.total_tokens, usage.estimated_cost_usd);
}
# Ok(())
# }
```

To interrupt that turn from elsewhere — a signal handler, a supervisor, a
timeout — clone the `SessionsAPI` and the `SessionId` and call:

```rust,no_run
# use polaris::sessions::{SessionId, SessionsAPI};
# use polaris_code::run;
# fn stop(sessions: &SessionsAPI, session_id: &SessionId) {
let was_running = run::request_interrupt(sessions, session_id);
# let _ = was_running;
# }
```
