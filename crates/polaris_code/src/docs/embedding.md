# Embedding the coding agent in another program

This is the starting point for a program that is **not** the `polaris`
binary and wants to drive a coding turn itself: an evaluation runner, a test
harness, a protocol server, another front end.

There is no `Harness` type, no builder and no `init()` for composing a server
yourself. (A program that wants the stock agent with additions uses
[`startup::Host`](crate::startup::Host) instead; see below.) Embedding is three
things in order:

1. compose the plugins you want on a [`Server`](polaris::system::server::Server);
2. create a session that carries an [`AgentConfig`](crate::agents::AgentConfig);
3. call [`run::drive_turn`](crate::run::drive_turn).

The run surface is three free functions — [`drive_turn`](crate::run::drive_turn),
[`drive_until`](crate::run::drive_until) and
[`request_interrupt`](crate::run::request_interrupt) — and they hold no state.
The composition root is yours, exactly as [`startup`](crate::startup) is the binary's.

If what you want is the `polaris` agent itself, with a few plugins or settings
added, you do not need to compose a server: see
[Starting polaris with plugins of your own](#starting-polaris-with-plugins-of-your-own).

`polaris_code` exposes the agent as a library package. Add it from crates.io
and compose the plugins your application needs; the dependency list below
matches the minimal embedder.

## Dependencies

Add these direct dependencies to the application that runs the full example:

```toml
[dependencies]
polaris_code = "0.4.3"
polaris = { package = "polaris-ai", version = "0.9.1", default-features = false, features = ["anthropic"] }
polaris_ontrace = { version = "0.3.0", default-features = false, features = ["oxigraph"] }
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
| **`ToolsPlugin` plus whichever capability plugins you want.** [`CodeToolsPlugin`](crate::tools::CodeToolsPlugin), [`CodeGraphPlugin`](crate::tools::CodeGraphPlugin), [`MemoryToolsPlugin`](crate::tools::MemoryToolsPlugin), `ShellPlugin`. | These are the agent's hands. All of them are optional: with none installed the turn still runs, the model simply has nothing to call. Each defaults to the `working_dir` you give it, and for a single session that directory should be the one in your `AgentConfig`. Each call the agent makes also carries a [`CallRoot`](crate::tools::CallRoot) built from `AgentConfig::working_dir`, and the file and shell tools work in that folder instead of the one they were built with, which is how one server can serve sessions in different folders: see [Running several isolated calls on one server](#running-several-isolated-calls-on-one-server). |
| **`PersistencePlugin`, `SessionsPlugin`, [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin), and ontrace's `OxigraphPlugin`.** | Sessions and their serializers, the agent graph itself, and the RDF store the agent reifies its decisions into. `OxigraphPlugin::in_memory()` is enough; the agent writes its trajectory there whether or not you read it. |
| **A [`TokenMeter`](crate::usage::TokenMeter) inserted as a global _before_ the first `create_context()`.** | Not optional, and the failure is quiet: the agent's `act` system takes the meter as a required `Res<TokenMeter>`, so a session context without one cannot run that step at all — the turn comes back `Completed` with `final_text: None` and `usage: None`, as if the model had said nothing. [`TurnOutcome::usage`](crate::run::TurnOutcome) reads the same meter back, and `usage: None` always means **unmeasured, not free**. |
| **A session carrying an `AgentConfig`, and then a [`PolicySnapshot`](crate::agents::PolicySnapshot).** | The config names the model and the working directory. The policy is what every control decision reads — see the warning below, it is not optional in practice. |

For a trusted disposable container, file tools can explicitly opt out of path
containment with `CodeToolsConfig::with_unrestricted_paths(true)`. Mirror that
choice with `AgentConfig::with_unrestricted_file_paths(true)` so the system
prompt describes the actual access. The agent setting only changes that prompt;
the tool configuration grants access. Edit permissions, exclusive file creation
and journal capture still apply. The CLI exposes this combination only through
`--dangerously-skip-permissions` in an unattended run.

To apply one output limit and one reasoning effort to every model call, register
the provider with [`ModelRunPlugin`](crate::model_run::ModelRunPlugin) and
[`ModelRunOptions`](crate::model_run::ModelRunOptions) instead of its ordinary
plugin. See [unattended runs](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/unattended-runs.md).

An optional [`RunTrajectory`](crate::trajectory::RunTrajectory) records native
provider calls as ATIF with shared message histories. Create it in the host
before any model call, put a clone in `ModelRunOptions::trajectory`, insert
another with `Server::insert_global` so tool results are recorded, and call
`finish` when the run ends. `finish` also writes the tool results and messages
recorded since the last model call; a hard kill before then can lose them. Use
[`rebuild_request`](crate::trajectory::rebuild_request) to reconstruct a call's
exact native input from its one-based step id, including compacted context.

## Model request timeouts

For Bedrock requests that need more than the Responses endpoint's default
60 seconds, set `--model-request-timeout-secs 600`,
`POLARIS_CODE_MODEL_REQUEST_TIMEOUT_SECS=600`, or
`model_request_timeout_secs = 600` in the user `config.toml` or project
`polaris.toml`. Profiles accept the same key. The usual precedence applies:
flags, environment, project file, user file, then defaults. Values must be
whole seconds from 1 to 86400; empty, fractional, zero and out-of-range values
are startup errors. Unset leaves all provider defaults unchanged.

The setting applies to Bedrock's Converse and Responses paths, including calls
from summaries, reviewers and topologies. Embedders can parse a
[`ModelRequestTimeout`](crate::providers::ModelRequestTimeout) and call
[`ReadyProvider::with_request_timeout`](crate::providers::ReadyProvider::with_request_timeout)
before either installation method. This sets the checked AWS SDK configuration's
`operation_attempt_timeout`; the Responses client reads its HTTP timeout from
that same configuration. Existing connection/read limits and retry settings
remain in force, so a shorter existing limit can still end a request earlier.

The limit bounds one transport attempt, rather than a complete turn or its
retries. Converse streaming bounds establishment of the response, not the event
stream after headers arrive. Bedrock Responses currently buffers the full HTTP
response before producing stream events, so its timeout covers that response.
The SDK and Responses adapter can retry internally; each attempt gets the same
limit. Their final transport error reaches the agent's existing retry policy
as a transient HTTP/network failure: Polaris 0.9.1 discards the native timeout
type, so the final report can say `network` rather than `timeout`. The agent
retries only while attempts remain and no visible output or billed usage has
committed the call. Its separate 300-second silence limit can fire first;
raising this transport limit does not raise that policy limit.

Anthropic and `OpenAI` expose no public request-timeout builder in Polaris 0.9.1.
Chat Completions exposes a per-read idle timeout, which does not bound a live
request's duration. Selecting any of those providers with this setting is
therefore refused at startup.

## What you get back

[`run::drive_turn`](crate::run::drive_turn) never panics and never returns an
error; a session-level failure comes back as
[`TurnReason::Failed`](crate::run::TurnReason::Failed). It returns one
[`TurnOutcome`](crate::run::TurnOutcome):

| Field | What an embedder does with it |
|-------|-------------------------------|
| `reason` | `Completed`, `Failed(failure)`, `Interrupted`, or `NoTopologyStarted`. A [`TurnFailure`](crate::agents::TurnFailure) carries the one message to show and its [`FailureKind`](crate::agents::FailureKind) (`auth`, `rate_limit`, `overloaded`, `timeout`, `network`, `invalid_request` or `other`); a transient failure (`rate_limit`, `overloaded`, `timeout`, `network`) has already been retried as `RetryPolicy` allows before it is reported, and `auth`, `invalid_request` and `other` are never retried, except that a reply that could not be read (`other`, from `GenerationError::InvalidResponse`) is sent again up to [`MALFORMED_REPLY_ATTEMPTS`](crate::agents::retry::MALFORMED_REPLY_ATTEMPTS) times in all, even after it showed text. [`TurnReason::exit_code`](crate::run::TurnReason::exit_code) maps it to the CLI contract (`0`/`1`/`130`) if you are exposing one. [`run::driver_continues`](crate::run::driver_continues) says whether a looping driver should start another turn. |
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
* **An installed approver parks the turn until you answer it.**
  `InteractiveApprovalPlugin` sends a question with `io_type = "confirm"` and then blocks on `receive()` —
  the same channel your prompt came in on. Under an attended posture your
  program *is* the human: reply with an `IOMessage` carrying metadata
  `confirmed = "true"`, or the turn waits forever. Dropping the input sender
  ends the wait as a refusal, not as an approval.
* **One front end, chosen at composition time.** You cannot embed the TUI
  inside another UI, and you cannot run the TUI panels and `ExecIOPlugin`
  together to get "both". Pick one when you build the `Server`.
* **The CLI is not part of the library.** `--print`, `--output-format`,
  `--session`, `--allow-edit`, the exit codes and the identity line live in
  [`startup`](crate::startup). [`exec::report`](crate::exec::report) *renders*
  those formats and is public, but nothing in the run surface prints: the
  startup module owns stdout. If you want `json` output, call
  [`RunReport`](crate::exec::RunReport) and print it yourself.
* **The `POLARIS_CODE_*` environment variables are the binary's.** The model,
  the graph-search mode, the prompt-cache strategy and the unattended nudge
  count (`POLARIS_CODE_UNATTENDED_NUDGES`, see
  [Continuing unattended replies](#continuing-unattended-replies-and-bounding-shell-output))
  are read from the environment by [`startup`](crate::startup) only. An embedder sets them on
  [`AgentConfig`](crate::agents::AgentConfig) (`with_graph_search`,
  `with_prompt_cache`) and on [`CodeGraphPlugin`](crate::tools::CodeGraphPlugin).
  The same config also carries the per-session settings `with_system_prompt`,
  `with_command_globs` and `with_max_output_tokens`, described under
  [Running several isolated calls on one server](#running-several-isolated-calls-on-one-server).
* **Terminal behaviour is front-end policy, not agent behaviour.** `/commands`,
  `@` mentions, session switching, rewind, background tasks and turn steering
  are wired by [`startup`](crate::startup) out of [`terminal`](crate::terminal)
  parts. An embedder
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

For attended confirmation, explicitly add
[`InteractiveApprovalPlugin`](crate::agents::InteractiveApprovalPlugin). It uses
that session's existing `UserIO` and offers the same scopes as the terminal.
Omitting the plugin denies calls that require a person, with a tool error
explaining that no approver is registered. Modes and explicit run grants that
allow calls without asking continue to work without the plugin.

## Continuing unattended replies and bounding shell output

Two parts of the per-turn [`PolicySnapshot`](crate::agents::PolicySnapshot)
decide things the `polaris` binary sets for you and an embedder must set itself:

* **Unattended replies.** In a run nobody watches, a reply that calls no tool
  ends the turn, even if it only says what the agent will do next.
  [`ContinuationPolicy`](crate::agents::ContinuationPolicy) can ask the model to
  go on instead. Its library default is **0 nudges**. The binary reads
  `POLARIS_CODE_UNATTENDED_NUDGES` and applies
  [`DEFAULT_UNATTENDED_NUDGES`](crate::agents::continuation::DEFAULT_UNATTENDED_NUDGES)
  (0, off) when it is unset; the library never reads that variable, so an embedder
  that wants the same behaviour sets
  [`with_unattended_nudges`](crate::agents::ContinuationPolicy::with_unattended_nudges)
  itself. Nudges are sent only when `policy.permission` is an unattended one,
  so set both. A reply cut off at the output limit is continued up to three
  times per turn whatever you set here.
* **Shell output size.** [`ObservationPolicy`](crate::agents::ObservationPolicy)
  sets how much of each `run_command` output stream the model sees, as a
  [`StreamView`](crate::tools::shell_render::StreamView).
  [`PolicySnapshot::for_agent`](crate::agents::PolicySnapshot::for_agent) and
  `defaults` bound each stream to 10,000 characters (the first and last 5,000).
  `StreamView::FULL` shows everything the shell captured.

Build the snapshot with `PolicySnapshot::for_agent`, not `defaults()`:
`CodeAgent::setup` already seeds the session with `for_agent(&config)`, which
sizes context projection and compaction to the `context_window` and
`max_output_tokens` in the session's `AgentConfig`, and `defaults()` would
replace that sizing with the unsized budgets. Insert the result after the
session is created and before the turn starts:

```rust,no_run
# use polaris::sessions::{SessionId, SessionsAPI};
# use polaris_code::agents::{AgentConfig, PermissionMode, PolicySnapshot, UnattendedGrants};
# use polaris_code::tools::shell_render::StreamView;
# async fn install_policy(
#     sessions: &SessionsAPI,
#     session_id: &SessionId,
#     config: &AgentConfig,
# ) -> Result<(), Box<dyn std::error::Error>> {
let mut policy = PolicySnapshot::for_agent(config);

// Nobody can answer this run: refuse what would need a person...
policy.permission = PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
// ...ask the model to go on once after a reply that calls no tool...
policy.continuation = policy.continuation.with_unattended_nudges(1);
// ...and retry a failed model call for about five minutes, as `polaris -p` does.
policy.retry = polaris_code::agents::policy::RetryPolicy::unattended();

// Show the model at most 4,000 characters of each shell output stream.
policy.observation.shell_stream = StreamView::bounded(4_000);

sessions
    .with_context(session_id, move |ctx| {
        ctx.insert(policy);
    })
    .await?;
# Ok(())
# }
```

The bounded text is what the model reads and what the tool block published to
your `UserIO` shows. The trajectory still records the executor's full result.

## Running several isolated calls on one server

A program that runs many agent calls, such as an evaluation runner or a job
queue, can compose one server and start one session per call. What a call may
use comes from its own session, so two calls on the same server need not share
a prompt, a folder, a command list or a budget:

| A call has its own | Set with | What it does |
|--------------------|----------|--------------|
| **Prompt** | [`AgentConfig::with_system_prompt`](crate::agents::AgentConfig::with_system_prompt) | Replaces the built-in system prompt in each request. The project grounding block is still appended. |
| **Tool folder** | [`AgentConfig::working_dir`](crate::agents::AgentConfig) | The agent's tool lanes put a [`CallRoot`](crate::tools::CallRoot) with this folder on every call. The file tools (`read_file`, `read_span`, `edit_file`, `create_file`, `list_files`, `search`, `glob`) refuse a path that leaves it. `run_command` and `run_in_background` start in it, and `run_command` refuses a `working_dir` that leaves it. |
| **Command list** | [`AgentConfig::with_command_globs`](crate::agents::AgentConfig::with_command_globs) | A shell command runs only if every command in it matches a pattern. An empty list gives the session no shell: the shell tools, including `task_output` and `stop_task`, are not offered to the model. A non-empty list is shown to the model in its prompt and in each refusal. |
| **Context window** | `AgentConfig::context_window` | Optional explicit model input-plus-output capacity. Configures context projection and compaction after reserving the output allowance and prompt overhead. See [unattended runs](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/unattended-runs.md). |
| **Output limit** | [`AgentConfig::with_max_output_tokens`](crate::agents::AgentConfig::with_max_output_tokens) | Bounds one reply, and is the output allowance the spend cap reserves for each request. Left unset, the request carries no limit from the agent and each reservation assumes the Anthropic fallback of 4096 tokens, whatever the provider. Set it to the limit your model provider was registered with, as `polaris` does. |
| **Spend cap** | [`TokenMeter::set_task_cost_cap`](crate::usage::TokenMeter::set_task_cost_cap) on a meter of the call's own | Before each request the agent reserves the most it could cost, and ends the call without contacting the model when that does not fit in what is left. [`TokenMeter::task_cost_spent_usd`](crate::usage::TokenMeter::task_cost_spent_usd) reads back what the cap counted as spent. See `docs/run-cost-ceiling.md`. |

Insert the call's own config, its own `UserIO` and its own meter when the
session is created. A meter inserted in the session shadows the server's global
one, so the server's meter is not charged for that call:

```rust,no_run
# use std::num::NonZeroU32;
# use std::sync::Arc;
# use polaris::plugins::UserIO;
# use polaris::sessions::{AgentTypeId, SessionId, SessionsAPI};
# use polaris_code::agents::{AgentConfig, CodeAgent};
# use polaris_code::exec::ExecIOProvider;
# use polaris_code::usage::TokenMeter;
# fn start_call(
#     sessions: &SessionsAPI,
#     io: Arc<ExecIOProvider>,
#     folder: std::path::PathBuf,
# ) -> Result<(SessionId, TokenMeter), Box<dyn std::error::Error>> {
let meter = TokenMeter::new();
meter.set_task_cost_cap(0.50)?;
let config = AgentConfig::new("anthropic/claude-sonnet-5", folder)
    .with_system_prompt("You review one file and report what is wrong with it.")
    .with_command_globs(vec!["cargo check*".to_string()])
    .with_max_output_tokens(NonZeroU32::new(4096).expect("non-zero"));

let session_id = SessionId::new();
let call_meter = meter.clone();
sessions.create_session_with(
    sessions.create_context(),
    &session_id,
    &AgentTypeId::from_name(CodeAgent::NAME),
    move |ctx| {
        ctx.insert(config.clone());
        ctx.insert(UserIO::new(io.clone()));
        ctx.insert(call_meter.clone());
    },
)?;
// Then insert the call's `PolicySnapshot` and drive the turn, as above.
# Ok((session_id, meter))
# }
```

What a call's folder and command list do **not** cover:

* **Only some tools follow the call's folder.** The file tools, `run_command`
  and `run_in_background` follow the [`CallRoot`](crate::tools::CallRoot).
  The code-graph, memory and MCP tools stay server-wide: they use the
  directory, index and connections the server's own plugins were built on,
  whichever call uses them. Do not register them on a server whose calls must
  stay apart.
* **The folder is where a command starts, not an operating-system sandbox.**
  A shell command that is allowed to run can still read or write any path the
  process can reach.
* **Background tasks belong to the folder they started in.** `task_output` and
  `stop_task` called from a session see only tasks started under the same
  folder: not another folder's, and not tasks the host started itself. Two
  sessions on the same folder see each other's tasks. A host call with no
  `CallRoot` sees all tasks.
* **The shell executor still applies.** Its allowed directories must include
  each call's folder, and its deny rules apply to every call.
* **A command list is text matching.** It decides which commands the agent
  starts. It is not a file or privilege boundary: a permitted program still
  runs with the process's OS-account access. See `docs/permission-modes.md`.
* **A spend cap works inside the turn only.** It is a different mechanism from
  the operator's `--max-cost-usd` ceiling; `docs/run-cost-ceiling.md` says how
  the two differ.

## Starting polaris with plugins of your own

A program that wants the `polaris` agent as it is, plus plugins of its own, does
not compose a server. It builds a [`startup::Host`](crate::startup::Host), adds
what it needs, and runs it on a command line. The host gets the binary's flags,
configuration files, environment variables, providers, both front ends and exit
codes, and no startup code is copied.

```no_run
use polaris_code::startup::{Host, clap};

#[tokio::main]
async fn main() {
    let code = Host::new()
        // A setting of the host's own, read from the command line or the
        // environment and listed in `--help` with the binary's.
        .arg(clap::Arg::new("experiment").long("experiment").env("MY_EXPERIMENT"))
        // Plugins, added after every plugin `polaris` adds and before the
        // server starts. They see the parsed command line.
        .plugins(|launch, _server| {
            let _name = launch.matches().get_one::<String>("experiment");
            // `_server.add_plugins(MyPlugin::new(_name))` goes here.
            Ok(())
        })
        .run(std::env::args_os())
        .await;
    std::process::exit(code);
}
```

Plugins added this way can do what any plugin can while the server builds. Two
seams exist for changing the agent's behaviour, and the startup wires both:

* a plugin that depends on
  [`PermissionRulesPlugin`](crate::agents::permission_rules::PermissionRulesPlugin)
  adds a rule written in Rust with
  [`PermissionRules::add_code_rule`](crate::agents::permission_rules::PermissionRules::add_code_rule);
* a plugin that depends on [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin)
  adds steps to the coding loop through
  [`CodeAgentSteps`](crate::agents::CodeAgentSteps). The startup registers the
  agent built from the finished registry, and the Graph tab draws it, so a host
  does neither itself.

With no plugin added, the agent is the stock `polaris` agent. The
[module documentation](crate::startup) gives the order startup adds its own
plugins in, which of them a host plugin may depend on, and a complete host with
a rule. `tests/host_startup.rs` runs a host with a rule and steps against a
scripted model.

A host setting is a command-line argument and, with `Arg::env`, an environment
variable. It is not a configuration-file key: the files supply defaults only for
`polaris`'s own settings. Startup does not let a host replace the model provider
the command line selects; a host that wants a different one composes its own
server as described above.

## Adding coding-agent graph steps

A host can let plugins extend the coding loop without replacing its graph.
Each contributing plugin depends on `CodeAgentPlugin` and fills the
[`CodeAgentSteps`](crate::agents::CodeAgentSteps) startup resource during
`build`. Its `after_tools` builders append ordinary graph systems after a full
tool round; its `before_finish` builders append systems that may call
[`FinishGate::hold`](crate::agents::FinishGate::hold) on a candidate answer.
Builders run by ascending priority, then stable namespaced key.

A host that starts through [`startup::Host`](crate::startup::Host) does this
already. A host that composes its own server reads
`server.get_global::<CodeAgentSteps>()` after `server.finish().await` and
registers [`CodeAgent::with_steps`](crate::agents::CodeAgent::with_steps) with
`SessionsAPI`, using `CodeAgent::NAME` for sessions as before. The registry is
frozen at startup; an empty registry builds exactly the stock graph. For a
Graph tab installed before startup, use
[`GraphViewPlugin::for_code_agent_steps`](crate::graph_view::GraphViewPlugin::for_code_agent_steps)
with `GraphPanelPlugin`; it publishes the complete graph during `ready`.
The [step-extension module](crate::agents::code_agent::steps) has a complete
plugin and host example, along with hold ordering and budget semantics.
