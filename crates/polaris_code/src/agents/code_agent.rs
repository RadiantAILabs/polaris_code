//! The reflective coding agent: a `ReAct` loop whose every control decision reads
//! a [`PolicySnapshot`] and is reified into the Session `ABox`.
//!
//! # Architecture
//!
//! ```text
//! receive_user_input → refresh_project_docs → init_loop → reset_continuations → loop("agent_loop") {
//!     compact_context → manual ? finish_compaction :
//!         prepare_context → begin_model_call → act → loop("model_retries") { … } →
//!             model_answered ? (accept_response →
//!                 has_tool_calls ? (review_tools → prepare_calls → tool_calls → finish_calls) :
//!                     (decide_continuation → continues ? continue_turn : finalize)) :
//!                 report_failure
//!     on error → recover
//! }
//! ```
//!
//! The model call's retry stage is described in [`retry`]: `act` is one
//! attempt, and a failed one is retried or given up on by graph edges.
//!
//! The object loop is the proven `ReAct` pattern (one LLM call per iteration; the
//! model's own output structure drives control flow). The *reflective* layer
//! adds: `prepare_context` assembles the window + injects [`ProjectContext`],
//! every decision point reads the per-turn [`PolicySnapshot`], and each decision
//! / tool call is written to the trajectory graph (write-only in v1).
//!
//! `refresh_project_docs` is the turn boundary's one grounding step: it re-reads
//! the discovered `AGENTS.md`/`CLAUDE.md` chain so a mid-session edit reaches the
//! model on the next turn, and changes nothing — leaving the cached prompt
//! prefix byte-identical — on the turns where no document moved.

/// Scan results with the rewritten text and the shapes that were withheld.
pub use crate::redaction::Redaction;

/// Credential scanning shared by tool output and service diagnostics.
///
/// # Scope
///
/// Global. The rule set is process configuration chosen once at the
/// composition root, not per-session state.
///
/// # Provider
///
/// [`CodeAgentPlugin`], from [`CodeAgentPlugin::with_redaction`]. The binary
/// gates it on `POLARIS_CODE_REDACT`; every other host gets it enabled.
///
/// # Access
///
/// Read as `Res<SecretRedactor>`. `execute_call` reads it and applies it to
/// each tool result — the one seam every consumer of that result is downstream
/// of. Scanning does not change the shared configuration: `scan` works on a
/// copy and returns the rewritten text.
///
/// # Alternatives
///
/// _none_ — there is one redaction rule set per server. Turn it off with
/// [`CodeAgentPlugin::with_redaction`] rather than replacing the resource.
///
/// # Example
///
/// ```
/// use polaris::system::{param::Res, system};
/// use polaris_code::agents::code_agent::SecretRedactor;
///
/// #[system]
/// async fn redact_output(redactor: Res<SecretRedactor>) -> String {
///     let scan = redactor.scan("ANTHROPIC_API_KEY=sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm");
///     scan.text().to_owned()
/// }
///
/// ```
pub use crate::redaction::SecretRedactor;

pub(crate) mod tool_calls;

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use super::auto_review::{
    AutoReviewPlugin, ReviewDecision, ReviewHistory, ReviewIntent, ReviewResult, review_tools,
};
use super::config::AgentConfig;
use super::context::{
    CacheAwareWindow, CompactionRun, ContextManager, LinearWindow, SummarizingWindow,
    compact_context, compaction_focus, finish_compaction,
};
use super::continuation::{
    Continuation, ContinuationLedger, continue_turn, decide_continuation, final_answer_text,
    reset_continuations,
};
use super::failure::{self, Attempts, TurnFailure};
use super::permission_rules::{
    ApprovalRequest, ApprovalScope, PermissionCall, PermissionRules, PermissionRulesPlugin,
    ReviewEligibility, RunToolGrants, SessionGrants, ShellApproval,
};
use super::policy::PolicySnapshot;
use super::project_context::ProjectContext;
use super::retry::{
    self, AttemptLedger, AttemptOutcome, RetryStep, abandon, accept_response, act,
    begin_model_call, decide_retry, report_failure, wait_to_retry,
};
use super::state::LoopState;
use super::trajectory::{Decision, record_tool_call};
use crate::terminal::attachments::{AttachmentStore, user_message};
use crate::terminal::background::{
    BackgroundTasks, COALESCE_TICK, background_body, background_receipt,
};
use crate::terminal::dispatch::is_session_switch_signal;
use crate::terminal::status;
use crate::terminal::transcript::{
    LiveTranscript, OUTCOME_COMPLETE, OUTCOME_ERROR, PHASE_BACKGROUND, PHASE_DENIED, PHASE_ERROR,
    PHASE_OK, PHASE_PENDING, PHASE_RUNNING, PHASE_START, ToolActivity, tool_detail,
};
use crate::terminal::{ContainedPrompt, SessionSettings, TurnPrompt, TurnSteering};
use crate::tools::shell_render::{render_run_command, run_command_outcome, run_command_succeeded};
use crate::tools::{
    CallRoot, CodeIndexer, RUN_IN_BACKGROUND, ToolPolicyRegistry, ViewedImage, code_index_state,
    enrich, tool_is_read_only,
};
use crate::usage::TokenMeter;

use futures_util::StreamExt;
use polaris::agent::{Agent, SetupError};
use polaris::graph::{CaughtError, Graph, RunCancellation};
use polaris::models::llm::{Llm, LlmResponse, Message, ToolCall, ToolResultContent, UserBlock};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IO_TYPE, IOContent, IOMessage, IOSource, PersistenceAPI, PersistencePlugin, UserIO,
};
use polaris::prelude::Out;
use polaris::sessions::SessionInfo;
use polaris::shell::{ShellCancel, ShellConfig, ShellSinkHandle};
use polaris::system::param::{ErrOut, Res, ResMut, SystemContext};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::prelude::SystemError;
use polaris::system::resource::{GlobalResource, LocalResource};
use polaris::system::server::Server;
use polaris::system::system;
use polaris::tools::{
    LlmRequestBuilderExt, ToolContext, ToolError, ToolPermission, ToolRegistry, ToolsPlugin,
};
use polaris_ontrace::oxigraph::OxigraphStore;
use polaris_tui::commands::{Availability, StatusReport, attachment_handles, report_status};

/// The current LLM instance, resolved in `setup` and injected as a local.
#[derive(Clone)]
pub struct AgentLlm(Llm);

impl Deref for AgentLlm {
    type Target = Llm;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl LocalResource for AgentLlm {}

/// The system prompt a session sends: its role text, or the built-in prompt when
/// the session was configured with none, followed by the project grounding
/// block.
///
/// A session limited to shell command patterns is told which ones, so its first
/// command can be one that runs.
fn system_prompt_for(config: &AgentConfig, project: &ProjectContext) -> String {
    let base = config.system_prompt.as_deref().unwrap_or(SYSTEM_PROMPT);
    let mut prompt = format!("{base}\n\n{}", project.system_prompt());
    if config.unrestricted_file_paths {
        prompt.push_str("\n\nFile access: this disposable-container run explicitly permits file tools to access any path reachable by the process, including absolute paths and paths outside the working directory. Relative paths start in the working directory.");
    }
    if let Some(globs) = config.command_globs.as_deref().filter(|g| !g.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(&command_patterns_note(globs));
    }
    prompt
}

/// What a model needs to know to write a shell command its session accepts.
///
/// A command runs only when it, and every command in a chain or pipe, matches
/// one of the session's patterns. A pattern with an argument slot does not
/// match the bare program name, and each call starts in the working folder, so
/// a `cd` has nothing to carry over.
fn command_patterns_note(globs: &[String]) -> String {
    let list = globs
        .iter()
        .map(|glob| format!("`{glob}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Shell commands: this session accepts only commands that match one of these \
         patterns: {list}. Chains, pipes and newline-separated commands are accepted \
         only when every command matches. Listed interpreters accept multiline quoted \
         arguments and scripts through a quoted-delimiter heredoc (for example \
         `python3 - <<'PY'`). Unquoted heredoc bodies must contain no dollar signs, \
         backticks or backslashes. Commands after the delimiter are checked too. Command \
         substitutions, complex expansions, displaced command words and unsupported shell syntax are refused. A pattern \
         with an argument slot needs an argument unless a bare-program pattern is \
         also listed. Each call starts in the working folder; a listed `cd` affects \
         only its own call."
    )
}

/// Assembled per-iteration context: the effective system prompt, the projected
/// message window to send this turn, and the turn marker.
#[derive(Clone)]
pub struct PreparedContext {
    /// Base prompt + project grounding block.
    pub system_prompt: String,
    /// Model label used for host-owned cost reporting after the call.
    pub model_id: String,
    /// The projected live window (bounded per `policy.retrieval`).
    pub messages: Vec<Message>,
    /// Whether to cache the stable prefix (system + tools) this turn.
    pub cache_prefix: bool,
    /// Message-index cache breakpoints for this window (see [`ProjectedWindow`]).
    ///
    /// [`ProjectedWindow`]: super::context::ProjectedWindow
    pub cache_breakpoints: Vec<usize>,
    /// Coarse turn marker used to stamp reified trajectory nodes.
    pub turn: usize,
}

const SYSTEM_PROMPT: &str = "\
You are a precise terminal coding agent operating in a sandboxed repository.

Follow the ReAct pattern (Reason + Act):
1. REASON in text before every action — state what you know, what you need, and why.
2. ACT by calling a tool to read code, search, navigate the code graph, edit a
   file, or run a command — or respond with your final answer.
3. OBSERVE the tool result and begin your next turn with fresh reasoning.

Work economically — every token you pull into context is re-sent on every later
turn, so locate precisely and read minimally:
- Navigate structure with the code-graph tools (find_definition, callers_of,
  callees_of, impls_of_trait), not text search — they resolve by symbol.
- The code index covers Rust, Python, JavaScript, and TypeScript. To understand
  a file in one of them, call `outline` for its map, then `read_symbol` (or
  `read_span`) for only the definitions you need. A whole-file `read_file` is a
  last resort; for an unindexed language or a large file, pass `offset`/`limit`.
- Query the codebase by concept, not just by name: `concepts`/`find_by_role`
  for roles (plugins, agents, tool handlers), `resources`/`accessors_of` for what
  reads or writes a given resource. These describe Rust/Polaris roles only, so
  they return nothing for a file in another indexed language.
- Never re-read what is already in context.
- Batch independent tool calls into one turn.

Do not wait on a slow command. `run_command` blocks the turn and is killed at
its short timeout, so a test suite, a build or a long-running server goes to
`run_in_background` instead: it returns a task number immediately and the
command keeps running across turns. Read it back with `task_output` — while it
runs for its latest output, after it ends for its full output and exit status,
including a task that finished while you were doing something else. End one
early with `stop_task`. A backgrounded command is permission-gated exactly as
the same command in the foreground, so backgrounding never avoids a prompt, and
never re-run a command you have already backgrounded.

Reuse conclusions: before a structural investigation, `recall_memory` to see if
you (or a past session) already answered it; once you reach a durable structural
conclusion, `memoize` it (question, claim, and the `path:start-end` spans it rests
on) so it can be reused — it auto-expires when the cited code changes.

Treat the task statement as the specification. Before changing code, list each
explicit requirement it states: exact error messages and output formats,
encodings, inputs to accept and to reject, and every edge case it names. Make
sure a test exercises each one — your own tests, not only the existing suite —
and before your final answer, go through the list again and confirm each
requirement against a test that passed.

Never call a tool without first explaining your reasoning.";

/// Most read-only tool calls from one batch that may be in flight at once.
///
/// The prompt asks the model to batch independent calls, so the width of a
/// batch is the model's choice, not ours; an unbounded fan-out would turn a
/// forty-call batch into forty simultaneous file handles and forty whole files
/// resident at once. Eight is chosen against what the batch is actually made
/// of: these calls are io-bound, so the win is saturating one disk's queue
/// depth rather than a core count. Eight covers typical batches in one wave
/// while staying far below the
/// blocking pool `tokio::fs` dispatches onto — the bound that matters here is
/// the host's descriptors and the window's memory, not the runtime's.
pub const MAX_CONCURRENT_READ_ONLY_CALLS: usize = 8;

async fn send_error(user_io: &UserIO, text: impl Into<String>) {
    let msg = IOMessage::new(IOContent::Text(text.into()), IOSource::System)
        .with_metadata(IO_TYPE, "error");
    let _ = user_io.send(msg).await;
}

/// The next item accepted by the agent's single input boundary.
enum AgentInput {
    /// A proof-carrying prompt from the host's turn-input queue.
    Prompt(TurnPrompt),
    /// A message from the decorated channel, after any local command work.
    Channel(Result<IOMessage, polaris::plugins::IOError>),
}

/// Receive user input and append model-bound text to history.
///
/// Steering adds a prompt source; it does not replace the decorated [`UserIO`]
/// channel. Both are polled at this one exhaustive boundary, which keeps host
/// command advertisement and dispatch live while prompt text arrives through
/// the queue. Adding a third source therefore requires adding an explicit arm
/// here instead of silently bypassing an existing host policy surface.
///
/// Both prompt sources hand their text to
/// [`user_message`](crate::terminal::user_message), which accepts only a
/// [`ContainedPrompt`] — so whichever way a prompt arrived, its `@` mentions
/// have been resolved against [`AgentConfig::working_dir`] before the model
/// can be asked about them. The steering queue carries the proof already,
/// because nothing unchecked can be submitted into it. The channel does not:
/// [`CommandDispatch`](crate::terminal::CommandDispatch) checks when a host
/// installs it, but a front end that wires a bare provider (the
/// non-interactive one does) has no decorator, so the check is made here.
#[system]
async fn receive_user_input(
    user_io: Res<UserIO>,
    mut context: ResMut<ContextManager>,
    mut state: ResMut<LoopState>,
    steering: Res<TurnSteering>,
    attachments: Res<AttachmentStore>,
    config: Res<AgentConfig>,
    cancel: Res<RunCancellation>,
    settings: Res<SessionSettings>,
    mut intent: ResMut<ReviewIntent>,
    mut reviews: ResMut<ReviewHistory>,
) -> Result<(), SystemError> {
    intent.begin_turn(settings.approval_generation());
    reviews.clear();
    let next = if steering.is_enabled() {
        cancel
            .run_until_cancelled(async {
                tokio::select! {
                    prompt = steering.receive_prompt() => AgentInput::Prompt(prompt),
                    received = user_io.receive() => AgentInput::Channel(received),
                }
            })
            .await
    } else {
        cancel
            .run_until_cancelled(async { AgentInput::Channel(user_io.receive().await) })
            .await
    };
    let Some(next) = next else {
        return Err(SystemError::ExecutionError("turn interrupted".into()));
    };

    match next {
        AgentInput::Prompt(prompt) => {
            intent.push(prompt.text.as_str());
            context.push(user_message(&attachments, prompt.text, &prompt.attachments));
        }
        AgentInput::Channel(received) => {
            match received {
                Ok(message) => {
                    if let Some(focus) = compaction_focus(&message) {
                        context.request_compaction(focus);
                        return Ok(());
                    }
                    if is_session_switch_signal(&message) {
                        state.is_complete = true;
                        return Err(SystemError::ExecutionError(
                            "host requested a session switch".into(),
                        ));
                    }
                    // The handles ride the message's metadata, not its content:
                    // `IOContent` carries one modality, and splitting a prompt and
                    // its image into two messages would let them land in different
                    // turns.
                    let handles = attachment_handles(&message);
                    if let IOContent::Text(text) = message.content {
                        // A refusal ends the turn rather than looping for another
                        // prompt. `CommandDispatch` loops, because on the channel
                        // it decorates there is a user still typing; here there
                        // may be no second prompt coming at all — the
                        // non-interactive front end enqueues exactly one — and
                        // waiting for one would hang instead of answering. So the
                        // reason is sent, and the turn ends without a model call:
                        // `--print` exits non-zero with it, and an interactive
                        // host has already refused at its own ingress.
                        let prompt = match ContainedPrompt::check(text, &config.working_dir) {
                            Ok(prompt) => prompt,
                            Err(rejected) => {
                                let reason = rejected.notice();
                                let _ = user_io.send(rejected.into_message()).await;
                                return Err(SystemError::ExecutionError(reason));
                            }
                        };
                        intent.push(prompt.as_str());
                        context.push(user_message(&attachments, prompt, &handles));
                    }
                }
                Err(err) => tracing::error!("failed to receive user input: {err}"),
            }
        }
    }
    Ok(())
}

/// Re-read the project instruction documents, once per turn.
///
/// This is the whole of the freshness seam: a user who edits `AGENTS.md`
/// mid-session to correct the agent is obeyed on the next turn rather than
/// after a restart. It sits *after* `receive_user_input` on purpose — an edit
/// made while the user was still typing their correction belongs to the turn
/// that correction starts — and outside the loop, so a turn's tool round-trips
/// all run against one set of documents.
///
/// Only the documents refresh; [`ProjectContext`]'s git probes deliberately do
/// not. The grounding block is the head of the cached prompt prefix, and
/// `git status --short` moves on nearly every turn of a coding session, so
/// re-running it here would spend a cache miss per turn to report something
/// nobody edited. [`ProjectContext::refresh_documents`] returns having changed
/// nothing when no document changed, which keeps that prefix byte-identical.
#[system]
async fn refresh_project_docs(
    config: Res<AgentConfig>,
    mut project: ResMut<ProjectContext>,
    user_io: Res<UserIO>,
) {
    project.refresh_documents(&config.working_dir).await;
    // Instructions reaching the model from above the directory the user chose
    // — or conventions dropped for the budget — are never honoured silently.
    if let Some(notice) = project.take_notice() {
        let _ = user_io
            .send(IOMessage::new(IOContent::Text(notice), IOSource::System))
            .await;
    }
}

/// Apply user steering only between completed graph steps.
///
/// This boundary runs before context projection and the next model call. A
/// message arriving during model or tool work waits here; completed tool
/// effects and their canonical result blocks are never rewound or duplicated.
#[system]
async fn apply_steering(
    steering: Res<TurnSteering>,
    attachments: Res<AttachmentStore>,
    mut context: ResMut<ContextManager>,
    mut intent: ResMut<ReviewIntent>,
) {
    for prompt in steering.take_steering() {
        intent.push(prompt.text.as_str());
        context.push(user_message(&attachments, prompt.text, &prompt.attachments));
    }
}

/// Initialize the agent loop.
async fn init_loop() -> LoopState {
    LoopState {
        is_complete: false,
        failure: None,
    }
}

/// Assemble the window for this iteration: inject [`ProjectContext`] and record
/// the retrieval decision. The compaction/budget seam lives here.
#[system]
async fn prepare_context(
    mut context: ResMut<ContextManager>,
    project: Res<ProjectContext>,
    mut policy: ResMut<PolicySnapshot>,
    mut llm: ResMut<AgentLlm>,
    models: Res<ModelRegistry>,
    settings: Res<SessionSettings>,
    store: Res<OxigraphStore>,
    config: Res<AgentConfig>,
    session: Res<SessionInfo>,
    user_io: Res<UserIO>,
) -> PreparedContext {
    // Commands can be queued while an approval is open, but this snapshot is
    // taken only after `receive_user_input` begins the next turn. The current
    // tool decision therefore keeps the policy it started with.
    //
    // Settings may move this turn onto another model, so status and cost report
    // the model the turn actually runs on. The id advances only where the swap
    // itself succeeds, keeping the report true when the registry refuses it.
    let mut model_id = config.model_id.clone();
    if let Some(active) = settings.turn_snapshot() {
        policy.permission = active.permission_mode.permission_policy();
        if let Ok(selected) = models.llm(&active.model_id) {
            llm.0 = selected;
            model_id = active.model_id;
        }
    }
    // Publish the resolved id before the call, so the host's accounting reads
    // the model that served this turn rather than the one the session was
    // configured with — they differ the moment `/model` has been used.
    settings.note_served(&model_id);
    // A session configured with no shell commands has no shell: the model is
    // not offered the tools that start, read or stop shell tasks.
    if config.command_globs.as_ref().is_some_and(Vec::is_empty) {
        for tool in super::policy::SHELL_EXECUTION_TOOLS
            .into_iter()
            .chain([crate::tools::tasks::TASK_OUTPUT])
        {
            if !policy
                .tool_palette
                .denied
                .iter()
                .any(|denied| denied == tool)
            {
                policy.tool_palette.denied.push(tool.to_string());
            }
        }
    }
    let turn = context.turn_marker();
    let system_prompt = system_prompt_for(&config, &project);

    context.set_model(&model_id);
    let total = context.messages.len();
    let projected = context.project(&policy.retrieval);

    // The policy passed here is the one this turn resolves every tool call
    // against a few systems later, including the rewrite the settings snapshot
    // may just have made. Reporting anything else would describe a posture the
    // session is not in.
    status::send(
        &user_io,
        &model_id,
        &config.working_dir,
        &context.status_session(session.session_id.as_str()),
        u64::from(session.turn_number),
        &policy.permission,
        projected.estimated_tokens,
        projected.token_budget,
        projected.compacted,
        // Read from the same index facts the navigation tools gate on, so the
        // row and the tool answers cannot disagree about whether the graph is
        // in a position to know anything.
        code_index_state(&store).phase,
    )
    .await;

    Decision {
        kind: "retrieval",
        intent: None,
        options: Vec::new(),
        chosen: format!("messages={}/{total}", projected.messages.len()),
        policy_refs: vec![
            format!("retrieval.max_messages={:?}", policy.retrieval.max_messages),
            format!("retrieval.max_tokens={:?}", policy.retrieval.max_tokens),
            format!(
                "compact_after_tokens={:?}",
                policy.retrieval.compact_after_tokens
            ),
            format!("cache_prefix={}", projected.cache_prefix),
            format!("cache_breakpoints={:?}", projected.breakpoints),
            format!("token_ratio={:.3}", context.token_ratio()),
            format!("last_input_tokens={:?}", context.last_prompt_tokens()),
        ],
    }
    .record(&store, turn);

    PreparedContext {
        system_prompt,
        model_id,
        messages: projected.messages,
        cache_prefix: projected.cache_prefix,
        cache_breakpoints: projected.breakpoints,
        turn,
    }
}

/// Whether this batch may be run concurrently rather than one call at a time.
///
/// Three conditions, and the batch is judged as a whole rather than sliced into
/// runs:
///
/// 1. **More than one call.** A single call uses the sequential graph lane;
///    routing it here would buy nothing and cost it the streaming and
///    backgrounding the loop gives it.
/// 2. **Every call is read-only.** Read-only-ness is the tool's declared
///    property, read here through [`tool_is_read_only`] — the classification
///    seam that sits with the toolsets, not a table in this loop. It is
///    fail-closed: a name it does not know is treated as mutating and the batch
///    falls back to sequential, so the worst an unclassified new tool can cost
///    is the latency the concurrent lane saves. `tests/read_only_declaration.rs` turns
///    that latency cost into a failing test the moment a registered tool goes
///    unclassified.
/// 3. **Every call is permitted outright.** A `Confirm` anywhere in the batch
///    means a human is in the loop, and interleaving one approval dialog with
///    calls racing behind it is exactly the confusion
///    `docs/parallel-tool-batches.md` rules out: confirmation must not become
///    racy. A `Deny` is cheap either
///    way.
///
/// Deliberately *not* attempted: splitting a mixed batch into a concurrent
/// read-only prefix and a sequential tail. That requires reasoning about which
/// reads commute with the following write — the batch `[read a, edit a,
/// read a]` is the counter-example — and the whole-batch test is worth more
/// than the residual win.
fn batch_runs_concurrently(calls: &[&ToolCall], permissions: &[ToolPermission]) -> bool {
    calls.len() > 1
        && calls
            .iter()
            .all(|call| tool_is_read_only(&call.function.name))
        && permissions
            .iter()
            .all(|permission| *permission == ToolPermission::Allow)
}

/// Run a batch of independent read-only calls concurrently, reporting in the
/// order the model asked for them.
///
/// The model is told to batch independent calls, and it does: four `read_span`s
/// in one assistant message used to cost four round trips of io latency because
/// the loop awaited each in turn. They are started together here instead, at
/// most [`MAX_CONCURRENT_READ_ONLY_CALLS`] in flight.
///
/// **Order is a property of the reporting, not of the racing.** Completion
/// order is whatever the disk decides; every observable — the `tool_result`
/// blocks pushed into the canonical history, the reified `ToolCall` nodes, and
/// the activity blocks the panel closes — is emitted afterwards by one loop
/// walking the batch in request order. Results land in a slot indexed by
/// request position, so nothing downstream can observe the race.
///
/// **Cancellation** is the turn's token racing the whole batch: losing drops
/// every in-flight future at its own await point, so an interrupt leaves
/// nothing running. What each call did is still known — the slots filled before
/// the drop hold real results — so the batch reports exactly what completed and
/// marks the rest interrupted, and every `tool_use` still gets its pair.
///
/// The invocations run with a [`ToolContext`] that carries the session's
/// [`CallRoot`] and nothing else. The other `#[context]` values this host
/// supplies are the shell sink and the detachable cancellation observer, and
/// the only tool that declares them is `run_command`, which is not read-only
/// and therefore never reaches this lane.
#[allow(
    clippy::too_many_arguments,
    reason = "the system's own resources, passed through rather than gathered into a struct \
              that would exist only to shorten this signature"
)]
async fn execute_read_only_batch(
    calls: &[&ToolCall],
    context: &mut ContextManager,
    tool_registry: &ToolRegistry,
    user_io: &UserIO,
    policy: &super::policy::PermissionPolicy,
    store: &OxigraphStore,
    live: &LiveTranscript,
    cancel: &RunCancellation,
    tasks: &BackgroundTasks,
    redactor: SecretRedactor,
    turn: usize,
    root: &std::path::Path,
) -> Result<(), SystemError> {
    // Every call in this batch resolved to `Allow`; reify that in request order
    // before anything runs, so the trajectory reads identically whichever lane
    // executed the batch.
    for call in calls {
        let mut recorded = permission_decision(policy, ToolPermission::Allow, "allow");
        if let Some((_, source)) = policy
            .rules
            .as_ref()
            .and_then(|rules| rules.resolve(&call.function.name, &call.function.arguments))
        {
            recorded
                .policy_refs
                .push(format!("authorization.source={source}"));
        }
        recorded.record(store, turn);
    }

    // Open every block up front, in request order: the panel should show the
    // whole batch in flight at once, and a block opened from inside a racing
    // future would order itself by the scheduler.
    let mut activities = Vec::with_capacity(calls.len());
    let mut invocations = Vec::with_capacity(calls.len());
    for call in calls {
        let name = &call.function.name;
        let detail = tool_detail(name, &call.function.arguments);
        activities.push(
            ToolActivity::start(user_io, &call.id, live.current_id(), name.clone(), &detail).await,
        );
        // `BackgroundTasks` describes one slot, so a batch claims it as a
        // batch: held until every call is done, released together. Nothing is
        // lost by the coarseness — `request` refuses any label that is not
        // `run_command`, and no read-only call is one.
        invocations.push(tasks.enter(&call.id, name, live.current_id()));
    }

    let tool_context = ToolContext::new().with(CallRoot::new(root));
    let mut outcomes: Vec<Option<Result<serde_json::Value, ToolError>>> =
        calls.iter().map(|_| None).collect();
    // Boxed to a concrete type: an iterator of opaque borrowing futures fed
    // straight into `buffer_unordered` makes rustc infer a higher-ranked `Send`
    // bound the `#[system]` wrapper then cannot satisfy.
    type SlotResult = (usize, Result<serde_json::Value, ToolError>);
    let running: Vec<Pin<Box<dyn Future<Output = SlotResult> + Send + '_>>> = calls
        .iter()
        .enumerate()
        .map(|(slot, call)| {
            let tool_context = &tool_context;
            Box::pin(async move {
                let result = tool_registry
                    .execute_with(&call.function.name, &call.function.arguments, tool_context)
                    .await;
                (slot, result)
            }) as Pin<Box<dyn Future<Output = SlotResult> + Send + '_>>
        })
        .collect();
    let finished = cancel
        .run_until_cancelled(async {
            let mut running = futures_util::stream::iter(running)
                .buffer_unordered(MAX_CONCURRENT_READ_ONLY_CALLS);
            while let Some((slot, result)) = running.next().await {
                outcomes[slot] = Some(result);
            }
        })
        .await;
    drop(invocations);

    let mut result_blocks = Vec::with_capacity(calls.len());
    for ((call, activity), outcome) in calls.iter().zip(activities).zip(outcomes) {
        let name = &call.function.name;
        let block = match outcome {
            Some(Ok(value)) => {
                if let Some((content, shown)) = viewed_image_result(redactor, name, &value) {
                    tracing::info!("Observation: {shown}");
                    activity.finish(user_io, PHASE_OK, &shown).await;
                    record_tool_call(store, turn, name, &call.function.arguments, "ok", &shown);
                    call.result(content)
                } else {
                    // The same seam the sequential lane applies, for the same
                    // reason: a read-only tool is exactly the kind that reads a
                    // `.env`, and this lane's results reach the provider, the
                    // transcript and the `ABox` through this string alone.
                    let output = redact_tool_output(
                        redactor,
                        name,
                        value
                            .as_str()
                            .map(String::from)
                            .unwrap_or_else(|| value.to_string()),
                    );
                    tracing::info!("Observation: {output}");
                    activity.finish(user_io, PHASE_OK, &output).await;
                    record_tool_call(store, turn, name, &call.function.arguments, "ok", &output);
                    call.result(ToolResultContent::Text(output))
                }
            }
            Some(Err(err)) => {
                let output = redact_tool_output(redactor, name, err.to_string());
                activity.finish(user_io, PHASE_ERROR, &output).await;
                record_tool_call(
                    store,
                    turn,
                    name,
                    &call.function.arguments,
                    "error",
                    &output,
                );
                call.error(ToolResultContent::Text(output))
            }
            None => {
                let output = "Interrupted by user".to_string();
                activity.finish(user_io, PHASE_ERROR, &output).await;
                record_tool_call(
                    store,
                    turn,
                    name,
                    &call.function.arguments,
                    "interrupted",
                    &output,
                );
                call.error(ToolResultContent::Text(output))
            }
        };
        result_blocks.push(block);
    }
    context.push(Message::User {
        content: result_blocks,
    });

    if finished.is_none() {
        return Err(SystemError::ExecutionError("turn interrupted".into()));
    }
    Ok(())
}

fn permission_decision(
    policy: &super::policy::PermissionPolicy,
    permission: ToolPermission,
    chosen: &str,
) -> Decision {
    Decision {
        kind: "permission",
        intent: None,
        options: vec!["allow".into(), "deny".into()],
        chosen: chosen.into(),
        policy_refs: vec![
            format!("permission.mode={}", policy.mode_label()),
            format!("permission={permission:?}"),
        ],
    }
}

/// The scope a person chose, as the word the trajectory records.
fn approval_scope_label(scope: ApprovalScope) -> &'static str {
    match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::Exact => "exact",
        ApprovalScope::Prefix => "prefix",
    }
}

/// Turn the shell executor's ambiguous response into an actual refusal when
/// the run has no human who could answer it.
fn unattended_shell_refusal(
    tool_name: &str,
    value: &serde_json::Value,
    permission: &super::policy::PermissionPolicy,
) -> Option<String> {
    // Both shell-starting tools, because both return the same
    // confirmation-required shape and an unattended run must not be able to
    // reach an unapproved command by asking for it in the background.
    (super::policy::SHELL_EXECUTION_TOOLS.contains(&tool_name)
        && permission.is_unattended()
        && value.get("status").and_then(serde_json::Value::as_str) == Some("confirmation_required"))
    .then(|| "Permission denied: shell command is outside the unattended grant".to_string())
}

impl GlobalResource for SecretRedactor {}

/// Applies `redactor` to one tool result, announcing the substitution to the
/// reader and to the log without putting the value in either.
///
/// This is the single seam: `execute_call` calls it once, and the redacted
/// string is what the transcript renders, what `record_tool_call` reifies,
/// what `tracing` observes, and what the provider-bound `UserBlock` carries.
fn redact_tool_output(redactor: SecretRedactor, tool: &str, output: String) -> String {
    let scan = redactor.scan(&output);
    let Some(notice) = scan.notice() else {
        return output;
    };
    tracing::warn!(
        tool,
        redacted = scan.count(),
        kinds = scan.kinds().join(","),
        "withheld secret-shaped values from a tool result"
    );
    let mut text = scan.into_text();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&notice);
    text
}

/// The picture `view_image` returned, as the result block's content, with the
/// summary shown live and recorded in the Session `ABox`; `None` when `value`
/// is not one.
///
/// Both result lanes call this first, so there is one rule for when the model
/// receives an image. It is `Some` only when the tool is `view_image` **and**
/// the value is that tool's typed payload ([`ViewedImage::from_result`] checks
/// both): another tool, an MCP server included, cannot return JSON that makes
/// the model receive an image, and `view_image` returning anything else is an
/// ordinary text result. Everything that gets `None` keeps the text path and its
/// secret redaction.
///
/// The line is `[image a.png, PNG, 34.0 KB]`. The transcript, log and Session
/// RDF/ABox `record_tool_call` get this summary. Saved session history and ATIF
/// exports (`--trajectory-path`, under `extra.image`) keep the whole encoded
/// image, without secret scanning. The summary is redacted because its path is
/// model text.
fn viewed_image_result(
    redactor: SecretRedactor,
    tool: &str,
    value: &serde_json::Value,
) -> Option<(ToolResultContent, String)> {
    let image = ViewedImage::from_result(tool, value)?;
    let shown = redact_tool_output(redactor, tool, image.summary());
    let block = image.into_block()?;
    Some((ToolResultContent::Image(block), shown))
}

/// Most diff lines carried in a confirm preview.
///
/// A backstop, not the display budget: the panel bounds the rows it draws and
/// `Ctrl-O` expands them. This only stops a pathological replacement from being
/// copied through the IO channel in full.
const MAX_DIFF_LINES: usize = 200;

/// Lines of unchanged context kept around each change.
const DIFF_CONTEXT: usize = 3;

/// A cancellation observer that relays this run's token until the invocation
/// detaches, and never afterwards.
///
/// The upstream supervisor takes one observer for a command's whole life and
/// races it against the child, so "stop relaying" has to be expressed inside
/// the observer rather than by swapping it out. The select is `biased` on
/// purpose: when a turn is cancelled at the same moment a run detaches, the
/// cancellation wins, because a run still inside the turn must die with it.
/// Once detachment wins, the future parks forever — a backgrounded command is
/// no longer part of any turn, and the one cooperative stop signal stops
/// applying to it. Ending it from then on is `/stop`, which is
/// [`polaris::shell::ShellExecutor::stop`] on that process
/// and nothing routed through turn cancellation.
fn detachable_cancel(
    cancel: &RunCancellation,
    detached: tokio::sync::watch::Receiver<bool>,
) -> ShellCancel {
    let token = cancel.clone();
    ShellCancel::from_fn(move || {
        let token = token.clone();
        let mut detached = detached.clone();
        async move {
            tokio::select! {
                biased;
                () = token.cancelled_owned() => {}
                () = async move {
                    // Resolves when the run leaves the turn — explicitly, or
                    // because the invocation holding the latch ended, by which
                    // point the command it guarded is already over.
                    while !*detached.borrow_and_update() {
                        if detached.changed().await.is_err() {
                            break;
                        }
                    }
                } => std::future::pending::<()>().await,
            }
        }
    })
}

/// Build a human-readable confirmation prompt for a tool invocation.
///
/// `edit_file` shows a unified diff of the requested replacement; `run_command`
/// shows the command; anything else shows the raw arguments.
///
/// The diff is emitted as **structured text, never as ANSI**. The panel
/// neutralises control characters before rendering, so an escape sequence would
/// reach the user as the literal `[32m` its `ESC` was stripped from; a real
/// `--- `/`+++ ` unified diff is instead recognised and styled by the panel's
/// own diff renderer. The confirmation contract is untouched: this is the body
/// of the same `UserIO::confirm` prompt, still answered by the same
/// `confirmed` metadata.
fn build_confirm_prompt(tool_name: &str, args: &serde_json::Value) -> String {
    match tool_name {
        "edit_file" => {
            let path = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown>");
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            format!(
                "Edit '{path}':\n{}\nApply this edit?",
                format_diff(path, old, new)
            )
        }
        "run_command" => {
            let cmd = args
                .get("command")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| args.to_string());
            format!("Run command:\n  {cmd}\n\nExecute this command?")
        }
        _ => format!("Tool '{tool_name}' requires confirmation.\nArgs: {args}\n\nProceed?"),
    }
}

/// Render `old` → `new` as a unified diff headed by `path`.
fn format_diff(path: &str, old: &str, new: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    let rendered = diff
        .unified_diff()
        .context_radius(DIFF_CONTEXT)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string();
    truncate_lines(rendered)
}

/// Bound a rendered diff, saying how much was cut rather than cutting silently.
///
/// Trailing hunks are dropped whole-line from the end; the `--- `/`+++ ` header
/// is at the top and therefore always survives, which is what keeps the result
/// recognisable as a diff to the panel.
fn truncate_lines(diff: String) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return diff;
    }
    let omitted = lines.len() - MAX_DIFF_LINES;
    let head = MAX_DIFF_LINES / 2;
    let tail = MAX_DIFF_LINES - head;
    format!(
        "{}\n… ({omitted} lines omitted) …\n{}",
        lines[..head].join("\n"),
        lines[lines.len() - tail..].join("\n"),
    )
}

/// Close the answer block and record the termination decision.
///
/// The text is already on screen — `act` streamed it delta by delta — so this
/// does not send it again. It sends the closing message carrying the
/// authoritative full text, which the panel substitutes for what it
/// accumulated. That reconciles a dropped or reordered delta without a second
/// copy of the answer appearing, and it is also what makes the block render as
/// finished rather than still being written.
#[system]
async fn finalize(
    decision: Out<LlmResponse>,
    prepared: Out<PreparedContext>,
    context: Res<ContextManager>,
    user_io: Res<UserIO>,
    store: Res<OxigraphStore>,
    mut live: ResMut<LiveTranscript>,
) -> LoopState {
    let text = final_answer_text(&context.messages).unwrap_or_else(|| decision.text());
    live.close(&user_io, &text, OUTCOME_COMPLETE).await;

    Decision {
        kind: "termination",
        intent: None,
        options: Vec::new(),
        chosen: "complete".into(),
        policy_refs: vec!["no_tool_calls".into()],
    }
    .record(&store, prepared.turn);

    LoopState {
        is_complete: true,
        failure: None,
    }
}

/// Recover from a caught error: surface it once, and where a model reply can
/// still help, ask for one.
///
/// A model call the retry stage already gave up on arrives here classified, in
/// [`LoopState::failure`], with the provider having had its whole retry budget.
/// Asking the same provider again would only repeat the failure — against a
/// rejected API key it would be a request that cannot succeed — so that failure
/// is reported as it stands. Any other failed system gets one model call to
/// answer the user from what the conversation holds, on the same bounded window a
/// normal turn sends.
#[system]
async fn recover(
    error: ErrOut<CaughtError>,
    mut context: ResMut<ContextManager>,
    llm: Res<AgentLlm>,
    meter: Res<TokenMeter>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    project: Res<ProjectContext>,
    policy: Res<PolicySnapshot>,
    settings: Res<SessionSettings>,
    config: Res<AgentConfig>,
    mut live: ResMut<LiveTranscript>,
    mut loop_state: ResMut<LoopState>,
) -> LoopState {
    // Whatever the failed system left open stops here. Its partial text stays
    // on screen, marked as ended in error rather than still arriving.
    live.close(&user_io, "", OUTCOME_ERROR).await;
    tracing::error!(
        system = error.system_name,
        node = %error.node_id,
        duration = ?error.duration,
        kind = %error.kind,
        "recover: {}",
        error.message
    );

    if let Some(failure) = loop_state.failure.clone() {
        send_error(&user_io, failure.message.clone()).await;
        loop_state.is_complete = true;
        return LoopState {
            is_complete: true,
            failure: Some(failure),
        };
    }

    let error_text = format!("{} failed: {}", error.system_name, error.message);
    send_error(&user_io, error_text.clone()).await;
    context.push(Message::user(format!("[System Error] {error_text}")));

    // Keep the same project grounding the loop's `prepare_context` injects, so a
    // recovery turn doesn't silently lose git/project context.
    let system_prompt = system_prompt_for(&config, &project);
    // The window a normal turn would send, not the whole canonical history: a
    // recovery after a context overflow must not be larger than the request
    // that overflowed.
    let window = context.project(&policy.retrieval);
    let mut builder = llm
        .builder()
        .system(system_prompt)
        .messages(window.messages)
        .with_registry(&tool_registry);
    if window.cache_prefix {
        builder = builder.cache_prefix();
    }
    builder = builder.cache_breakpoints(window.breakpoints);

    meter.record_model_turn();
    let failure = match retry::within(policy.retry.attempt_timeout, builder.generate()).await {
        Ok(response) => {
            // The recovery reply is not streamed — it comes from a single
            // `generate` — so it is emitted as one already-closed block, which
            // renders exactly like a streamed answer that has finished.
            let text = response.text();
            live.whole(&user_io, &text, OUTCOME_COMPLETE).await;
            context.push(Message::assistant(text));
            let _ = user_io
                .send(report_status(
                    &StatusReport::default().with_availability(Availability::Available),
                ))
                .await;
            None
        }
        Err(err) => {
            // The recovery call itself also failed, so there is no assistant
            // answer for this turn, and none is invented. Carry the original
            // failure forward with the reason the model could not help, so the
            // turn is reported as failed instead of completed — otherwise the
            // graph reaching its terminal `is_complete` state here reads as
            // success even though nothing answered the user.
            let served = settings
                .served_model()
                .unwrap_or_else(|| config.model_id.clone());
            let cause = failure::describe(&err, &served, Attempts::default());
            tracing::error!(kind = %cause.kind, "recover: recovery call failed: {err}");
            send_error(&user_io, cause.message.clone()).await;
            Some(TurnFailure::new(
                cause.kind,
                format!("{error_text}. {}", cause.message),
            ))
        }
    };

    // The loop's own termination check reads this function's *return* value
    // (the graph's per-run output store, cleared every turn), which
    // `run_one_turn` cannot see afterward — it reads the *resource* instead
    // (persistent, queried via `ctx.get_resource::<LoopState>()`). Writing
    // both keeps the loop's iteration control and the turn's post-hoc read in
    // sync; without this, `run_one_turn` would still find whatever default
    // `LoopState` its own turn setup inserted, holding no failure at all.
    loop_state.is_complete = true;
    loop_state.failure = failure.clone();

    LoopState {
        is_complete: true,
        failure,
    }
}

/// The iteration limit of the model-call retry loop.
///
/// Not the retry budget: [`RetryPolicy::max_attempts`] ends the loop long
/// before this, and a graph cannot read policy when it is built. Every loop
/// needs a limit, and the executor's default is the turn limit, which is about
/// something else and must not cut a retry short.
///
/// [`RetryPolicy::max_attempts`]: super::policy::RetryPolicy::max_attempts
const MODEL_RETRIES_BOUND: usize = u32::MAX as usize;

/// Default loop bound for hosts that do not select a different turn limit.
/// The CLI's explicit unrestricted container mode overrides this default.
///
/// A real repository task often needs dozens of read, search and edit steps,
/// so the default leaves room for them while still bounding a runaway loop.
pub const DEFAULT_MAX_TURNS: usize = 100;

/// The reflective coding agent.
#[derive(Debug, Clone, Default)]
pub struct CodeAgent;

impl CodeAgent {
    /// Stable agent name.
    pub const NAME: &'static str = "CodeAgent";
}

impl Agent for CodeAgent {
    fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
        let config = ctx
            .get_resource::<AgentConfig>()
            .map_err(SetupError::new)?
            .clone();
        if let Some(window) = config.context_window {
            let output = config
                .max_output_tokens
                .unwrap_or(polaris::models::anthropic::DEFAULT_MAX_TOKENS);
            if window.get().saturating_sub(output.get()) < 2 {
                return Err(SetupError::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "context_window must exceed max_output_tokens by at least two tokens",
                )));
            }
        }
        if ctx.get_resource::<SessionSettings>().is_err() {
            ctx.insert(SessionSettings::compatibility_default(
                config.model_id.clone(),
            ));
        }
        let llm = ctx
            .get_resource::<ModelRegistry>()
            .map_err(SetupError::new)?
            .llm(&config.model_id)
            .map_err(SetupError::new)?;
        ctx.insert(AgentLlm(llm));
        let trajectory = ctx
            .get_resource::<crate::trajectory::RunTrajectory>()
            .ok()
            .map(|trace| trace.clone());
        if let Ok(mut context) = ctx.get_resource_mut::<ContextManager>() {
            context.set_trajectory(trajectory);
        }

        // Build per-session project grounding and the policy snapshot. These are
        // synchronous to keep `setup` non-async; ProjectContext shells out, so we
        // block on a short-lived runtime handle.
        let project = futures_block_on(ProjectContext::discover(&config.working_dir));
        ctx.insert(project);
        ctx.insert(PolicySnapshot::for_agent(&config));
        ctx.insert(LiveTranscript::default());
        ctx.insert(AttemptLedger::default());
        ctx.insert(tool_calls::ToolBatch::default());
        ctx.insert(tool_calls::CallLifetime::default());
        tool_calls::setup_cleanup(ctx);
        // Bind the queue's containment root to the very directory this agent
        // is configured with, so the gate the composer submits through and the
        // sandbox the tools run in can never disagree about what is inside the
        // workspace. A host that wired its own queue shares interior state
        // with the handle the panel holds, so binding here reaches it; one
        // that wired none gets a bound default.
        let host_wired_a_queue = match ctx.get_resource::<TurnSteering>() {
            Ok(steering) => {
                steering.bind_working_dir(&config.working_dir);
                true
            }
            Err(_) => false,
        };
        if !host_wired_a_queue {
            let steering = TurnSteering::default();
            steering.bind_working_dir(&config.working_dir);
            ctx.insert(steering);
        }
        // A host that wires the terminal shares *its* store here, so a handle
        // the composer minted is redeemable. Every other host gets an empty
        // one: `receive_user_input` reads it unconditionally, and a prompt
        // that names no handle never asks it for anything.
        if ctx.get_resource::<AttachmentStore>().is_err() {
            ctx.insert(AttachmentStore::new());
        }
        // A host that runs managed commands shares its own table here, so the
        // chord, the commands and this system all address the same runs. Every
        // other host gets one that reports to nobody: `execute_call` reads it
        // unconditionally, and a run nothing ever backgrounds only ever costs
        // it a bounded tail.
        if ctx.get_resource::<BackgroundTasks>().is_err() {
            ctx.insert(BackgroundTasks::detached(Duration::from_secs(
                ShellConfig::default().default_timeout_secs,
            )));
        }
        Ok(())
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(receive_user_input);
        graph.add_system(refresh_project_docs);
        graph.add_system(init_loop);
        graph.add_system(reset_continuations);

        graph.add_loop::<LoopState, _, _>(
            "agent_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(apply_steering);
                g.add_system(compact_context);
                g.add_conditional_branch::<CompactionRun, _, _, _>(
                    "manual_compaction",
                    |run| run.manual,
                    |manual| {
                        manual.add_system(finish_compaction);
                    },
                    |normal| {
                        normal.add_system(prepare_context);
                        normal.add_system(begin_model_call);
                        normal.add_system(act);
                        // A failed attempt is retried here, not inside `act`:
                        // the policy decides, a branch routes, and the edge
                        // back into `act` is the retry. The loop's own bound is
                        // `RetryPolicy::max_attempts`, which `decide_retry`
                        // enforces; the iteration limit below only exists
                        // because every loop needs one, and sits above any
                        // policy so `--max-turns` never shortens a retry.
                        normal.add_loop::<AttemptOutcome, _, _>(
                            "model_retries",
                            AttemptOutcome::is_settled,
                            |retry| {
                                retry.add_system(decide_retry);
                                retry.add_conditional_branch::<RetryStep, _, _, _>(
                                    "retry_or_give_up",
                                    |step| matches!(step, RetryStep::Retry { .. }),
                                    |again| {
                                        again.add_system(wait_to_retry);
                                        again.add_system(act);
                                    },
                                    |stop| {
                                        stop.add_system(abandon);
                                    },
                                );
                            },
                        );
                        if let Some(retries) = normal.last_node() {
                            normal.set_loop_max_iterations(retries, MODEL_RETRIES_BOUND);
                        }
                        normal.add_conditional_branch::<AttemptOutcome, _, _, _>(
                            "model_answered",
                            AttemptOutcome::is_success,
                            |answered| {
                                answered.add_system(accept_response);
                                answered.add_conditional_branch::<LlmResponse, _, _, _>(
                                    "has_tool_calls",
                                    |response| response.has_tool_calls(),
                                    |tool_branch| {
                                        tool_branch.add_system(review_tools);
                                        tool_calls::build_tool_calls(tool_branch);
                                    },
                                    |done_branch| {
                                        // A reply without a tool call ends the
                                        // turn unless the continuation policy
                                        // says it is not an answer yet.
                                        done_branch.add_system(decide_continuation);
                                        done_branch
                                            .add_conditional_branch::<Continuation, _, _, _>(
                                                "continues",
                                                Continuation::continues,
                                                |go_on| {
                                                    go_on.add_system(continue_turn);
                                                },
                                                |finish| {
                                                    finish.add_system(finalize);
                                                },
                                            );
                                    },
                                );
                            },
                            |unanswered| {
                                unanswered.add_system(report_failure);
                            },
                        );
                    },
                );
                g.add_error_handler(|h| {
                    h.add_system(recover);
                });
            },
        );
    }

    fn name(&self) -> &'static str {
        Self::NAME
    }
}

/// Block on a future from within the synchronous `setup`, regardless of the
/// Tokio runtime flavor `setup` happens to be called under.
///
/// `block_in_place` + `Handle::block_on` only works on a multi-thread runtime
/// (it panics on a current-thread runtime), but [`CodeAgent`] is a reusable
/// `pub` agent that must not constrain how the host `Server` configures its
/// runtime. Running the future to completion on a dedicated thread with its own
/// short-lived current-thread runtime is safe whether or not the caller is
/// already inside a runtime. This is a one-shot, setup-time cost.
fn futures_block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build setup-time runtime for project discovery")
                    .block_on(fut)
            })
            .join()
            .expect("project-discovery thread panicked")
    })
}

/// Plugin that registers the [`CodeAgent`]'s local resources and persistence.
///
/// Add it to run the reflective coding loop on a server that already has
/// models, tools and persistence.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ContextManager`] | local | Full history and summary checkpoints (persisted). |
/// | [`LoopState`] | local | Loop termination flag. |
/// | [`ContinuationLedger`] | local | Per-turn continuation counts, reset at the start of each turn. |
/// | [`PolicySnapshot`] | local | Per-turn decision policy (defaults in v1). |
/// | [`SecretRedactor`] | global | Withholds recognisable credentials from every consumer of a tool result. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | `HooksAPI` | Inserted only when no earlier plugin installed it, so the failure hook below can register. |
/// | `MiddlewareAPI` | Inserted only when no earlier plugin installed it, so the middleware below can register. |
///
/// # Dependencies
///
/// Expects [`ModelsPlugin`], [`ToolsPlugin`] and (for persistence)
/// [`PersistencePlugin`] to be present on the server; resolves the model and
/// project context in `setup`.
///
/// Two more dependencies are installed with their defaults when omitted:
///
/// - [`PermissionRulesPlugin`] supplies authorization resources. Its default
///   loads no rule files; add `PermissionRulesPlugin::new(root)` to load them.
/// - [`AutoReviewPlugin`] supplies the reviewer's `ReviewPolicy` and the local
///   state that attended `auto` mode reads. Its default is
///   `ReviewPolicy::default()`; add it with your own policy to change the
///   reviewer model, timeout, output cap or denial cutoff.
///
/// Human input is optional and is never installed by this plugin. Add
/// [`InteractiveApprovalPlugin`](super::human_approval::InteractiveApprovalPlugin)
/// to serve attended confirmation requests; without it, calls that need a
/// person receive an explicit denial and other calls continue.
///
/// Both depend on [`ToolPolicyPlugin`](crate::tools::ToolPolicyPlugin), which is
/// added by default as well, so tool capabilities can publish policy facts the
/// agent reads.
///
/// # Hooks Registered
///
/// An `OnGraphFailure` hook (`polaris_code::tool_batch_cleanup`) closes a tool
/// batch that an interruption left open, so every `tool_use` gets a result.
///
/// # Middleware Registered
///
/// Graph-execution middleware (`polaris_code::tool_batch_cleanup`) delivers the
/// terminal closures that the hook queued, once graph execution has finished.
///
/// # Lifecycle
///
/// - `build` registers the local resources, the hook and the middleware, and
///   publishes [`SecretRedactor`].
/// - `ready` registers the [`ContextManager`] serializer with `PersistenceAPI`
///   when [`PersistencePlugin`] provides it.
///
/// # Extends
///
/// - [`PersistencePlugin`]: adds a serializer for [`ContextManager`].
/// - The shared hook and middleware APIs: adds one hook and one middleware.
///
/// # Example
///
/// ```no_run
/// use polaris::models::ModelsPlugin;
/// use polaris::plugins::PersistencePlugin;
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code::agents::CodeAgentPlugin;
/// use polaris_code::agents::auto_review::{AutoReviewPlugin, ReviewPolicy};
/// use polaris_code::agents::permission_rules::PermissionRulesPlugin;
///
/// let mut server = Server::new();
/// server
///     .add_plugins(ModelsPlugin)
///     .add_plugins(ToolsPlugin)
///     .add_plugins(PersistencePlugin)
///     .add_plugins(PermissionRulesPlugin::new("."))
///     .add_plugins(AutoReviewPlugin::new(ReviewPolicy::default()))
///     .add_plugins(CodeAgentPlugin::new());
/// ```
pub struct CodeAgentPlugin {
    /// Whether to project with the cache-aware strategy ([`CacheAwareWindow`])
    /// and request provider prompt caching. The binary mirrors
    /// [`AgentConfig::prompt_cache`] here; defaults to `true`.
    prompt_cache: bool,
    /// The redaction configuration published as `Res<SecretRedactor>`.
    /// Enabled by default: a host has to opt *out* of withholding secrets.
    redaction: SecretRedactor,
}

impl Default for CodeAgentPlugin {
    fn default() -> Self {
        Self {
            prompt_cache: true,
            redaction: SecretRedactor::default(),
        }
    }
}

impl CodeAgentPlugin {
    /// A plugin with prompt caching enabled (the default).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable or disable the cache-aware context strategy + prompt caching
    /// (config gate). Disabled uses the sliding [`LinearWindow`]. Both compose
    /// [`SummarizingWindow`] for graph-owned checkpoints.
    ///
    /// [`LinearWindow`]: super::context::LinearWindow
    #[must_use]
    pub fn with_prompt_cache(mut self, on: bool) -> Self {
        self.prompt_cache = on;
        self
    }

    /// Turn secret redaction of tool output on or off (on by default).
    ///
    /// Off is a real escape hatch: a redactor that eats output the model needs
    /// is worse than none, so an operator who hits a false positive can get
    /// the raw bytes back. It is not a security setting to leave off — see
    /// [`SecretRedactor`].
    #[must_use]
    pub const fn with_redaction(mut self, on: bool) -> Self {
        self.redaction = SecretRedactor::new(on);
        self
    }
}

impl Plugin for CodeAgentPlugin {
    const ID: &'static str = "polaris_code::code_agent";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![
            PluginId::of::<ModelsPlugin>(),
            PluginId::of::<ToolsPlugin>(),
            PluginId::of::<PersistencePlugin>(),
            PluginId::of::<PermissionRulesPlugin>(),
            PluginId::of::<AutoReviewPlugin>(),
        ]
    }

    fn default_dependencies(&self) -> polaris::system::plugin::DefaultDependencies {
        super::auto_review::default_dependencies()
    }

    fn build(&self, server: &mut Server) {
        if self.prompt_cache {
            server.register_local(|| {
                ContextManager::with_strategy(Arc::new(SummarizingWindow::new(Arc::new(
                    CacheAwareWindow,
                ))))
            });
        } else {
            server.register_local(|| {
                ContextManager::with_strategy(Arc::new(SummarizingWindow::new(Arc::new(
                    LinearWindow,
                ))))
            });
        }
        tool_calls::install_cleanup(server);
        server.register_local(LoopState::default);
        server.register_local(ContinuationLedger::default);
        server.register_local(PolicySnapshot::defaults);
        // Process configuration, not session state: one rule set decided at
        // the composition root and read by every turn of every session.
        server.insert_global(self.redaction);
    }

    async fn ready(&self, server: &mut Server) {
        if let Some(api) = server.api::<PersistenceAPI>() {
            api.register::<ContextManager>(Self::ID);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panel strips terminal control characters before rendering, so an
    /// `ESC` in a preview reaches the user as the bare `[32m` it introduced.
    /// A preview must therefore carry no escape sequences at all.
    #[test]
    fn an_edit_preview_carries_no_ansi() {
        let args = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = 2;",
        });
        let prompt = build_confirm_prompt("edit_file", &args);
        assert!(
            !prompt.contains('\u{1b}'),
            "an escape would render as literal `[32m` once neutralised: {prompt}"
        );
        assert!(!prompt.contains("[0m"), "{prompt}");
    }

    /// The panel recognises a preview as a diff by its `--- `/`+++ ` header
    /// pair and styles it from there. Without them it renders as flat text.
    #[test]
    fn an_edit_preview_is_a_recognisable_unified_diff() {
        let args = serde_json::json!({
            "path": "src/lib.rs",
            "old_string": "let x = 1;",
            "new_string": "let x = 2;",
        });
        let prompt = build_confirm_prompt("edit_file", &args);
        assert!(prompt.contains("--- a/src/lib.rs"), "{prompt}");
        assert!(prompt.contains("+++ b/src/lib.rs"), "{prompt}");
        assert!(prompt.contains("-let x = 1;"), "{prompt}");
        assert!(prompt.contains("+let x = 2;"), "{prompt}");
        assert!(prompt.contains("Apply this edit?"), "{prompt}");
    }

    #[test]
    fn a_huge_replacement_is_bounded_but_keeps_its_diff_header() {
        let old_text = (0..5_000)
            .map(|n| format!("line {n}\n"))
            .collect::<String>();
        let new_text = (0..5_000)
            .map(|n| format!("changed {n}\n"))
            .collect::<String>();
        let diff = format_diff("big.rs", &old_text, &new_text);
        let rows = diff.lines().count();
        assert!(rows <= MAX_DIFF_LINES + 1, "unbounded at {rows} rows");
        // The header is at the top, so bounding from the end always keeps it —
        // which is what leaves the result recognisable as a diff.
        assert!(diff.starts_with("--- a/big.rs"), "{}", &diff[..80]);
        assert!(diff.contains("lines omitted"));
        assert!(diff.contains("-line 0"), "head missing: {diff}");
        assert!(diff.contains("+changed 4999"), "tail missing: {diff}");
    }

    #[test]
    fn a_command_preview_stays_the_command() {
        let args = serde_json::json!({ "command": "cargo test --workspace" });
        let prompt = build_confirm_prompt("run_command", &args);
        assert!(prompt.contains("cargo test --workspace"), "{prompt}");
        // Not a diff: it must not acquire diff headers and get styled as one.
        assert!(!prompt.contains("--- a/"), "{prompt}");
    }

    #[test]
    fn a_generic_confirmation_preview_names_the_tool_and_arguments() {
        let args = serde_json::json!({ "subject": "src/lib.rs" });
        let prompt = build_confirm_prompt("future_mutating_tool", &args);
        assert!(prompt.contains("future_mutating_tool"), "{prompt}");
        assert!(prompt.contains("src/lib.rs"), "{prompt}");
        assert!(prompt.ends_with("Proceed?"), "{prompt}");
    }

    #[test]
    fn unattended_turns_an_ambiguous_shell_response_into_a_refusal() {
        use crate::agents::{PermissionMode, UnattendedGrants};

        let unattended = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["cargo *".into()],
        })
        .permission_policy();
        let response = serde_json::json!({ "status": "confirmation_required" });
        assert!(unattended_shell_refusal("run_command", &response, &unattended).is_some());

        let attended = PermissionMode::Confirm.permission_policy();
        assert!(unattended_shell_refusal("run_command", &response, &attended).is_none());
    }

    #[test]
    fn permission_decisions_record_the_active_mode_as_provenance() {
        use crate::agents::PermissionMode;

        let policy = PermissionMode::AutoEdit.permission_policy();
        let decision = permission_decision(&policy, ToolPermission::Allow, "allow");
        assert_eq!(decision.kind, "permission");
        assert_eq!(decision.chosen, "allow");
        assert!(
            decision
                .policy_refs
                .contains(&"permission.mode=auto-edit".to_string())
        );
    }

    #[test]
    fn agent_name_is_stable() {
        assert_eq!(CodeAgent.name(), "CodeAgent");
        assert_eq!(CodeAgent::NAME, "CodeAgent");
    }

    #[test]
    fn build_produces_a_valid_graph() {
        let mut graph = Graph::new();
        CodeAgent.build(&mut graph);
        // Exercise the composition: the loop, the has_tool_calls branch, and the
        // error handler must wire into a structurally valid graph.
        assert!(graph.validate().is_ok(), "graph: {:?}", graph.validate());
    }

    #[test]
    fn build_registers_a_context_manager_in_both_cache_modes() {
        // Exercise both arms of the `prompt_cache` gate in `build()`: each must
        // register a `ContextManager` local (cache-aware vs. the sliding default).
        let mut server = Server::new();
        CodeAgentPlugin::new()
            .with_prompt_cache(true)
            .build(&mut server);
        assert!(server.has_local::<ContextManager>());

        let mut server = Server::new();
        CodeAgentPlugin::new()
            .with_prompt_cache(false)
            .build(&mut server);
        assert!(server.has_local::<ContextManager>());
    }

    #[test]
    fn the_result_the_model_reads_says_that_something_was_withheld() {
        let raw = "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI-K7MDENG-bPxRfiCYEXAMPLEKEY\n";
        let out = redact_tool_output(SecretRedactor::new(true), "read_file", raw.to_string());
        assert!(!out.contains("wJalrXUtnFEMI"), "{out}");
        assert!(out.contains("[REDACTED:secret-assignment]"), "{out}");
        assert!(
            out.contains("polaris-code redacted 1 secret-shaped value(s)"),
            "the model is told the bytes are not the bytes on disk: {out}"
        );
        // A clean result is returned untouched, notice and all.
        let clean = "fn main() {}\n";
        assert_eq!(
            redact_tool_output(SecretRedactor::new(true), "read_file", clean.to_string()),
            clean
        );
    }

    /// The rule both result lanes share for when the model receives a picture.
    mod viewed_image {
        use super::*;
        use crate::tools::VIEW_IMAGE;
        use polaris::models::llm::{DocumentSource, ImageMediaType};

        const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0";

        fn payload(path: &str) -> serde_json::Value {
            serde_json::to_value(ViewedImage::new(path, "image/png", PNG)).unwrap()
        }

        #[test]
        fn view_image_with_its_typed_payload_gives_the_model_a_picture() {
            let (content, shown) =
                viewed_image_result(SecretRedactor::new(true), VIEW_IMAGE, &payload("a.png"))
                    .expect("view_image's own payload is a picture");
            let ToolResultContent::Image(block) = content else {
                panic!("the model is given an image block");
            };
            assert!(matches!(block.media_type, ImageMediaType::PNG));
            let DocumentSource::Base64(data) = &block.data;
            assert_eq!(data, &crate::images::base64(PNG));
            assert_eq!(shown, "[image a.png, PNG, 29 B]");
        }

        #[test]
        fn the_line_shown_and_recorded_never_holds_the_encoded_picture() {
            let value = payload("a.png");
            let encoded = value["data"].as_str().unwrap().to_string();
            let (_, shown) = viewed_image_result(SecretRedactor::new(true), VIEW_IMAGE, &value)
                .expect("a picture");
            assert!(!shown.contains(&encoded), "{shown}");
            assert!(shown.len() < 80, "one short line: {shown}");
        }

        #[test]
        fn another_tool_returning_the_same_payload_gets_no_picture() {
            let value = payload("a.png");
            for tool in [
                "read_file",
                "run_command",
                "mcp__shots__view_image",
                "View_Image",
                "view_image ",
                "",
            ] {
                assert!(
                    viewed_image_result(SecretRedactor::new(true), tool, &value).is_none(),
                    "{tool:?} must not be able to hand the model a picture"
                );
            }
        }

        #[test]
        fn view_image_returning_anything_else_stays_text() {
            let redactor = SecretRedactor::new(true);
            for value in [
                serde_json::json!("view_image failed: not an image"),
                serde_json::json!({"path": "a.png", "data": "AAAA"}),
                serde_json::json!({"kind": "polaris_code.viewed_image"}),
                serde_json::json!({
                    "kind": "something_else", "path": "a.png",
                    "media_type": "image/png", "bytes": 1, "data": "AA=="
                }),
                serde_json::json!({
                    "kind": "polaris_code.viewed_image", "path": "a.png",
                    "media_type": "image/svg+xml", "bytes": 1, "data": "AA=="
                }),
                serde_json::json!({
                    "kind": "polaris_code.viewed_image", "path": "a.png",
                    "media_type": "image/png", "bytes": 1, "data": "AA==", "extra": true
                }),
                serde_json::json!({
                    "kind": "polaris_code.viewed_image", "path": "a.png",
                    "media_type": "image/png", "bytes": 99_999_999u64, "data": "AA=="
                }),
            ] {
                assert!(
                    viewed_image_result(redactor, VIEW_IMAGE, &value).is_none(),
                    "{value} is not a picture"
                );
            }
        }

        #[test]
        fn the_path_in_the_shown_line_is_redacted_like_any_text() {
            let value = payload("OPENAI_API_KEY=sk-proj-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2.png");
            let (_, shown) = viewed_image_result(SecretRedactor::new(true), VIEW_IMAGE, &value)
                .expect("a picture");
            assert!(!shown.contains("sk-proj-0Aa1"), "{shown}");
            let line = shown.lines().next().expect("image summary");
            assert!(line.starts_with("[image "), "{shown}");
            assert!(line.ends_with(" PNG, 29 B]"), "{shown}");
        }
    }

    /// `recover` as the error handler of a system that fails, run through the
    /// real session and executor — the one way to reach it that does not need
    /// a wiring fault in the agent's own graph.
    mod recovery {
        use std::sync::Mutex;

        use polaris::models::llm::{
            AssistantBlock, GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream,
            StopReason, Usage,
        };
        use polaris::plugins::{IOProvider, PersistenceAPI, PersistencePlugin};
        use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};

        use super::*;
        use crate::agents::failure::FailureKind;
        use crate::agents::policy::DEFAULT_MAX_MESSAGES;
        use crate::exec::{ExecIOPlugin, ExecIOProvider};

        /// What the scripted provider saw, and what it does with the one
        /// request recovery makes.
        struct Recorder {
            requests: Mutex<Vec<Vec<Message>>>,
            fail_with: Option<fn() -> GenerationError>,
        }

        struct Handle(Arc<Recorder>);

        impl LlmProvider for Handle {
            fn name(&self) -> &'static str {
                "anthropic"
            }

            async fn generate(
                &self,
                _model: &str,
                request: LlmRequest,
            ) -> Result<LlmResponse, GenerationError> {
                self.0.requests.lock().unwrap().push(request.messages);
                match self.0.fail_with {
                    Some(fail) => Err(fail()),
                    None => Ok(LlmResponse {
                        content: vec![AssistantBlock::text("the model's answer")],
                        usage: Usage::default(),
                        stop_reason: StopReason::EndTurn,
                        id: None,
                        model: None,
                    }),
                }
            }

            async fn stream(
                &self,
                _model: &str,
                _request: LlmRequest,
            ) -> Result<LlmStream, GenerationError> {
                Err(GenerationError::Http("recovery does not stream".into()))
            }
        }

        struct HandlePlugin(Arc<Recorder>);

        impl Plugin for HandlePlugin {
            const ID: &'static str = "test::recovery_provider";
            const VERSION: Version = Version::new(0, 1, 0);

            fn dependencies(&self) -> Vec<PluginId> {
                vec![PluginId::of::<ModelsPlugin>()]
            }

            fn build(&self, server: &mut Server) {
                server
                    .get_resource_mut::<ModelRegistry>()
                    .expect("ModelsPlugin builds first")
                    .register_llm_provider(Handle(self.0.clone()));
            }
        }

        #[tokio::test]
        async fn setup_rejects_windows_without_input_room_and_applies_valid_limits() {
            use std::num::NonZeroU32;

            let workspace = tempfile::tempdir().unwrap();
            let mut server = Server::new();
            server
                .add_plugins(ModelsPlugin)
                .add_plugins(HandlePlugin(Arc::new(Recorder {
                    requests: Mutex::new(Vec::new()),
                    fail_with: None,
                })));
            server.finish().await.unwrap();
            for output in [None, NonZeroU32::new(128)] {
                let allowance = output
                    .unwrap_or(polaris::models::anthropic::DEFAULT_MAX_TOKENS)
                    .get();
                for window in [
                    allowance - 1,
                    allowance,
                    allowance + 1,
                    allowance + 2,
                    allowance + 100,
                ] {
                    let mut config = AgentConfig::new("anthropic/test-model", workspace.path())
                        .with_context_window(NonZeroU32::new(window).unwrap());
                    config.max_output_tokens = output;
                    let mut ctx = server.create_context();
                    ctx.insert(config);
                    let result = CodeAgent.setup(&mut ctx);
                    if window < allowance + 2 {
                        let error = result.unwrap_err().to_string();
                        assert!(
                            error.contains("context_window must exceed max_output_tokens by at least two tokens"),
                            "{error}"
                        );
                        assert!(ctx.get_resource::<PolicySnapshot>().is_err());
                    } else {
                        result.unwrap();
                        let policy = ctx.get_resource::<PolicySnapshot>().unwrap();
                        let budget = ((window - allowance) * 4 / 5) as usize;
                        assert_eq!(policy.retrieval.max_tokens, Some(budget));
                        assert_eq!(
                            policy.retrieval.compact_after_tokens,
                            Some((budget / 2).max(1))
                        );
                        assert_eq!(
                            policy.retrieval.summarize_after_tokens,
                            Some((budget * 3 / 4).max(1))
                        );
                        assert_eq!(
                            policy.retrieval.summary_output_tokens.get(),
                            allowance.min(2_048)
                        );
                    }
                }
            }
        }

        #[tokio::test]
        async fn prepare_context_sets_the_model_before_counting_reasoning() {
            use polaris::graph::GraphExecutor;
            use polaris::models::llm::ReasoningBlock;
            use polaris::sessions::SessionInfo;

            for (model_id, tokens) in [
                ("aws.bedrock/global.openai.gpt-oss-120b-1:0", 1),
                ("anthropic/test-model", 4),
            ] {
                let workspace = tempfile::tempdir().unwrap();
                let (io, _input, _output) = ExecIOProvider::new(16);
                let mut server = Server::new();
                server.insert_global(OxigraphStore::in_memory().unwrap());
                server
                    .add_plugins(ModelsPlugin)
                    .add_plugins(HandlePlugin(Arc::new(Recorder {
                        requests: Mutex::new(Vec::new()),
                        fail_with: None,
                    })));
                server.finish().await.unwrap();
                // prepare_context uses the already resolved handle when no turn
                // snapshot selected a different one.
                let llm = server
                    .get_global::<ModelRegistry>()
                    .unwrap()
                    .llm("anthropic/test-model")
                    .unwrap();
                let mut ctx = server.create_context();
                ctx.insert(AgentConfig::new(model_id, workspace.path()));
                ctx.insert(AgentLlm(llm));
                ctx.insert(SessionSettings::compatibility_default(model_id));
                ctx.insert(SessionInfo {
                    session_id: SessionId::new(),
                    turn_number: 0,
                });
                ctx.insert(UserIO::new(Arc::new(io)));
                ctx.insert(ProjectContext::default());
                ctx.insert(PolicySnapshot::defaults());
                let mut context = ContextManager::default();
                context.messages = vec![Message::Assistant {
                    id: None,
                    content: vec![
                        AssistantBlock::Reasoning(ReasoningBlock {
                            id: None,
                            reasoning: vec!["αβ".into(), "last".into()],
                            signature: None,
                        }),
                        AssistantBlock::text("ok"),
                    ],
                }];
                ctx.insert(context);
                let mut graph = Graph::new();
                graph.add_system(prepare_context);
                GraphExecutor::new()
                    .execute(&graph, &mut ctx, None, None)
                    .await
                    .unwrap();
                assert_eq!(
                    ctx.get_resource::<ContextManager>()
                        .unwrap()
                        .project(&Default::default())
                        .estimated_tokens,
                    tokens,
                    "{model_id}",
                );
            }
        }

        #[system]
        async fn broken() -> Result<(), SystemError> {
            Err(SystemError::ExecutionError("the step broke".into()))
        }

        /// The coding agent's own session setup, around a graph whose only
        /// step fails and is handled by `recover`.
        struct BrokenStep;

        impl Agent for BrokenStep {
            fn setup(&self, ctx: &mut SystemContext<'static>) -> Result<(), SetupError> {
                CodeAgent.setup(ctx)
            }

            fn build(&self, graph: &mut Graph) {
                graph.add_system(broken).add_error_handler(|handler| {
                    handler.add_system(recover);
                });
            }

            fn name(&self) -> &'static str {
                "BrokenStep"
            }
        }

        /// What one recovery left behind.
        struct Outcome {
            requests: Vec<Vec<Message>>,
            failure: Option<TurnFailure>,
            shown: Vec<IOMessage>,
        }

        async fn run_recovery(
            fail_with: Option<fn() -> GenerationError>,
            history: usize,
        ) -> Outcome {
            let recorder = Arc::new(Recorder {
                requests: Mutex::new(Vec::new()),
                fail_with,
            });
            let workspace = tempfile::tempdir().unwrap();
            let data_dir = tempfile::tempdir().unwrap();
            let (front_end, _input, mut output) = ExecIOProvider::new(64);
            let front_end = Arc::new(front_end);

            let mut server = Server::new();
            server.insert_global(TokenMeter::new());
            server
                .add_plugins(ModelsPlugin)
                .add_plugins(HandlePlugin(recorder.clone()))
                .add_plugins(ToolsPlugin)
                .add_plugins(PersistencePlugin)
                .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                    data_dir.path().to_str().unwrap(),
                ))))
                .add_plugins(CodeAgentPlugin::new())
                .add_plugins(ExecIOPlugin::new(front_end.clone()));
            server.finish().await.unwrap();

            let persistence = server.api::<PersistenceAPI>().unwrap();
            let sessions = server.api::<SessionsAPI>().unwrap();
            sessions.set_serializers(persistence.serializers());
            sessions.register_agent(BrokenStep).unwrap();

            let id = SessionId::new();
            let config = AgentConfig::new("anthropic/test-model", workspace.path().to_path_buf());
            sessions
                .create_session_with_executor(
                    server.create_context(),
                    &id,
                    &AgentTypeId::from_name("BrokenStep"),
                    polaris::graph::GraphExecutor::new(),
                    move |ctx| {
                        ctx.insert(config.clone());
                    },
                )
                .unwrap();
            sessions
                .with_context(&id, |ctx| {
                    let mut context = ctx.get_resource_mut::<ContextManager>().unwrap();
                    for turn in 0..history / 2 {
                        context
                            .messages
                            .push(Message::user(format!("question {turn}")));
                        context
                            .messages
                            .push(Message::assistant(format!("answer {turn}")));
                    }
                })
                .await
                .unwrap();

            let drain = tokio::spawn(async move {
                let mut shown = Vec::new();
                while let Some(message) = output.recv().await {
                    shown.push(message);
                }
                shown
            });
            sessions.process_turn(&id).await.unwrap();
            let failure = sessions
                .with_context(&id, |ctx| {
                    ctx.get_resource::<LoopState>()
                        .ok()
                        .and_then(|state| state.failure.clone())
                })
                .await
                .unwrap();
            front_end.close().await;
            let shown = drain.await.unwrap();
            let requests = recorder.requests.lock().unwrap().clone();
            Outcome {
                requests,
                failure,
                shown,
            }
        }

        fn shown_text(outcome: &Outcome) -> Vec<(bool, String)> {
            outcome
                .shown
                .iter()
                .filter_map(|message| match &message.content {
                    IOContent::Text(text) => Some((
                        message.metadata.get(IO_TYPE).map(String::as_str) == Some("error"),
                        text.clone(),
                    )),
                    _ => None,
                })
                .collect()
        }

        fn text_of(message: &Message) -> String {
            match message {
                Message::User { content } => content
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect(),
                Message::Assistant { .. } => String::new(),
            }
        }

        #[tokio::test]
        async fn recovery_sends_the_bounded_window_not_the_whole_history() {
            let history = 4 * DEFAULT_MAX_MESSAGES;
            let outcome = run_recovery(None, history).await;

            assert_eq!(outcome.requests.len(), 1, "one recovery request");
            let sent = &outcome.requests[0];
            assert!(
                sent.len() <= DEFAULT_MAX_MESSAGES,
                "recovery sent {} messages; a normal turn sends at most {DEFAULT_MAX_MESSAGES}",
                sent.len()
            );
            assert!(sent.len() < history, "far fewer than the {history} held");
            let note = text_of(sent.last().expect("a non-empty window"));
            assert!(
                note.starts_with("[System Error] broken failed: ")
                    && note.ends_with("the step broke"),
                "the window still ends with the failure the model is to answer: {note}"
            );
            assert!(
                outcome.failure.is_none(),
                "the model answered, so the turn did"
            );
            let shown = shown_text(&outcome);
            assert!(
                shown.iter().any(|(_, text)| text == "the model's answer"),
                "{shown:?}"
            );
            assert_eq!(
                shown.iter().filter(|(error, _)| *error).count(),
                1,
                "the failed step is reported once: {shown:?}"
            );
        }

        #[tokio::test]
        async fn a_failed_recovery_reports_one_typed_failure_and_invents_no_reply() {
            let outcome = run_recovery(Some(|| GenerationError::Auth("denied".into())), 0).await;

            assert_eq!(
                outcome.requests.len(),
                1,
                "recovery is one request, not retried"
            );
            let failure = outcome
                .failure
                .clone()
                .expect("the turn is reported failed");
            assert_eq!(failure.kind, FailureKind::Auth);
            assert!(
                failure.message.contains("broken failed: ")
                    && failure.message.contains("the step broke. "),
                "the original failure is kept: {}",
                failure.message
            );
            assert!(
                failure
                    .message
                    .ends_with("Anthropic rejected the API key. Check the key saved by `polaris login anthropic` or supplied through ANTHROPIC_API_KEY."),
                "{}",
                failure.message
            );

            let shown = shown_text(&outcome);
            assert_eq!(
                shown.iter().filter(|(error, _)| *error).count(),
                2,
                "what failed, and why nothing could help: {shown:?}"
            );
            for stale in ["[System Error]", "LLM error", "I encountered an error"] {
                assert!(
                    shown.iter().all(|(_, text)| !text.contains(stale)),
                    "{stale:?} was shown: {shown:?}"
                );
            }
        }
    }
}
