//! The reflective coding agent: a `ReAct` loop whose every control decision reads
//! a [`PolicySnapshot`] and is reified into the Session `ABox`.
//!
//! # Architecture
//!
//! ```text
//! receive_user_input → refresh_project_docs → init_loop → loop("agent_loop") {
//!     prepare_context → act → has_tool_calls ? execute_tools : finalize
//!     on error → recover
//! }
//! ```
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

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use super::config::AgentConfig;
use super::context::{CacheAwareWindow, ContextManager};
use super::policy::{PolicySnapshot, ToolPalettePolicy};
use super::project_context::ProjectContext;
use super::state::LoopState;
use super::trajectory::{Decision, record_tool_call};
use crate::terminal::attachments::{AttachmentStore, user_message};
use crate::terminal::background::{
    BackgroundTasks, COALESCE_TICK, ToolRun, background_body, background_receipt,
};
use crate::terminal::dispatch::is_session_switch_signal;
use crate::terminal::status;
use crate::terminal::transcript::{
    LiveTranscript, OUTCOME_COMPLETE, OUTCOME_ERROR, PHASE_BACKGROUND, PHASE_DENIED, PHASE_ERROR,
    PHASE_OK, PHASE_PENDING, PHASE_RUNNING, PHASE_START, ToolActivity, forward, tool_detail,
};
use crate::terminal::{ContainedPrompt, SessionSettings, TurnPrompt, TurnSteering};
use crate::tools::shell_render::render_run_command;
use crate::tools::{CodeIndexer, RUN_IN_BACKGROUND, code_index_state, enrich, tool_is_read_only};
use crate::usage::TokenMeter;

use futures_util::StreamExt;
use polaris::agent::{Agent, SetupError};
use polaris::graph::{CaughtError, Graph, RunCancellation};
use polaris::models::llm::{
    Llm, LlmResponse, Message, StreamEvent, StreamEventExt, ToolCall, ToolResultContent, UserBlock,
};
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
use polaris_tui::commands::{
    Availability, StatusReport, attachment_handles, report_status, report_unavailable,
};

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

/// Report an *increment* of token usage as a `io_type=tokens` delta. The
/// conversation panel adds each delta's `in`/`out` counts into a running session
/// tally, so during a streaming call we forward the change since the last
/// emission (not the cumulative total the stream reports) and the sum still
/// matches cost.
async fn send_token_delta(user_io: &UserIO, in_delta: u64, out_delta: u64) {
    let msg = IOMessage::from_agent("code", IOContent::Text(String::new()))
        .with_metadata(IO_TYPE, "tokens")
        .with_metadata("in", in_delta.to_string())
        .with_metadata("out", out_delta.to_string());
    let _ = user_io.send(msg).await;
}

/// Fold a cumulative usage reading into the increment to emit this step.
///
/// Streams report *cumulative* `input`/`output` token totals, but the panel
/// tally and the session [`TokenMeter`] are additive, so each step forwards only
/// the increase since the previous reading. A repeated or decreasing reading
/// (which would otherwise double-count or underflow) yields `(0, 0)`.
fn token_increment(prev_in: u64, prev_out: u64, in_now: u64, out_now: u64) -> (u64, u64) {
    (
        in_now.saturating_sub(prev_in),
        out_now.saturating_sub(prev_out),
    )
}

/// Assemble the reified `tool_palette` [`Decision`] for a turn: the tools
/// actually offered (`offered`, already intersected with exposure), how the
/// palette was selected, and the policy values that drove it.
///
/// Split out of [`act`] so the provenance shape is unit-testable without
/// standing up the full system. Consumes `offered` into the decision's options.
fn tool_palette_decision(
    palette: &ToolPalettePolicy,
    offered: Vec<String>,
    max_attempts: u32,
    attempts_used: u32,
) -> Decision {
    let mode = palette.mode.label();
    let count = offered.len();
    Decision {
        kind: "tool_palette",
        intent: None,
        chosen: format!("{mode}:{count}"),
        options: offered,
        policy_refs: vec![
            format!("tool_palette.mode={mode}"),
            format!("tool_palette.denied={}", palette.denied.len()),
            format!("retry.max_attempts={max_attempts}"),
            format!("attempts_used={attempts_used}"),
        ],
    }
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
) -> Result<(), SystemError> {
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
            context.push(user_message(&attachments, prompt.text, &prompt.attachments));
        }
        AgentInput::Channel(received) => {
            match received {
                Ok(message) => {
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
) {
    for prompt in steering.take_steering() {
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
    context: Res<ContextManager>,
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
    let turn = context.turn_marker();
    let system_prompt = format!("{SYSTEM_PROMPT}\n\n{}", project.system_prompt());

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
        session.session_id.as_str(),
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

/// Single LLM call with tools available, with in-system retry per policy.
#[system]
async fn act(
    prepared: Out<PreparedContext>,
    mut context: ResMut<ContextManager>,
    llm: Res<AgentLlm>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
    meter: Res<TokenMeter>,
    mut live: ResMut<LiveTranscript>,
    cancel: Res<RunCancellation>,
) -> Result<LlmResponse, SystemError> {
    let messages = prepared.messages.clone();
    let max_attempts = policy.retry.max_attempts.max(1);

    // Per-turn tool selection (the agent narrowing its own palette). The predicate
    // composes the policy's allow-list/denylist with the registry's static
    // exposure inside `definitions_for`; we capture the resulting set for the
    // reified `tool_palette` decision below.
    let offered: Vec<String> = tool_registry
        .definitions_for(|name| policy.tool_palette.includes(name))
        .into_iter()
        .map(|def| def.name)
        .collect();
    if offered.is_empty() {
        // An empty palette is a legitimate pure-reasoning turn (the model is
        // sent no tools), but it is always intentional — log it so it is never
        // silent, and so an accidentally over-narrow policy is observable.
        tracing::debug!(
            mode = policy.tool_palette.mode.label(),
            "act: tool palette is empty — offering the model no tools this turn"
        );
    }
    // Cheap Arc clone of the session meter to fold usage into from the stream
    // closure; the binary reads it on exit to print a cost summary.
    let meter = TokenMeter::clone(&meter);

    let mut attempt = 0;
    // The loop yields the response plus the provider-native `input_tokens` for the
    // window we actually sent, captured on the successful attempt to calibrate the
    // context manager's token estimate.
    let (response, prompt_tokens) = loop {
        attempt += 1;

        // Per-attempt live token accounting. Each `MessageDelta`/`MessageStop`
        // carries *cumulative* usage; we forward the *increment* since the last
        // emission, because the conversation panel adds deltas into its running
        // tally. `committed` flips true once we've emitted anything this attempt;
        // past that point the additive tally can't be rolled back, so a failed
        // stream is terminal rather than retryable.
        let last_in = Arc::new(AtomicU64::new(0));
        let last_out = Arc::new(AtomicU64::new(0));
        let last_cache_read = Arc::new(AtomicU64::new(0));
        let last_cache_creation = Arc::new(AtomicU64::new(0));
        let committed = Arc::new(AtomicBool::new(false));
        // A fresh transcript block per attempt. A retry that has shown nothing
        // reuses nothing; a retry after visible text cannot happen at all
        // (emitting a delta marks the attempt committed, below), so one block
        // never mixes two attempts' text.
        // Close the previous iteration's block first: a ReAct turn emits one
        // block per iteration (reasoning, then its tools), and an unclosed one
        // would render as still being written.
        live.close(&user_io, "", OUTCOME_COMPLETE).await;
        let block = live.open();

        let user_io_ref: &UserIO = &user_io;
        let result = cancel
            .run_until_cancelled(async {
                // Cache-aware projection: the strategy decides whether to
                // cache the stable prefix (system + tools) and where to place the
                // rolling/stable conversation breakpoints; we forward those onto the
                // request so the provider emits `cache_control`.
                let mut builder = llm
                    .builder()
                    .with_registry_filtered(&tool_registry, |name| {
                        policy.tool_palette.includes(name)
                    })
                    .system(prepared.system_prompt.clone())
                    .messages(messages.clone());
                if prepared.cache_prefix {
                    builder = builder.cache_prefix();
                }
                builder = builder.cache_breakpoints(prepared.cache_breakpoints.clone());
                meter.record_model_turn();
                let stream = builder.stream().await?;

                // Emit token increments as usage events arrive, then hand the
                // (still complete) event stream to the collector for assembly.
                stream
                    .then(|event| {
                        let block = block.clone();
                        let last_in = last_in.clone();
                        let last_out = last_out.clone();
                        let last_cache_read = last_cache_read.clone();
                        let last_cache_creation = last_cache_creation.clone();
                        let committed = committed.clone();
                        let meter = meter.clone();
                        async move {
                            // Forward the model's text as it arrives; anything
                            // that reached the screen makes the attempt terminal.
                            if forward(&block, user_io_ref, &event).await {
                                committed.store(true, Ordering::Relaxed);
                            }
                            let usage = match &event {
                                Ok(StreamEvent::MessageDelta { usage }) => Some(usage),
                                Ok(StreamEvent::MessageStop { usage, .. }) => Some(usage),
                                _ => None,
                            };
                            if let Some(usage) = usage {
                                let in_now = usage.input_tokens.unwrap_or(0);
                                let out_now = usage.output_tokens.unwrap_or(0);
                                let (in_delta, out_delta) = token_increment(
                                    last_in.swap(in_now, Ordering::Relaxed),
                                    last_out.swap(out_now, Ordering::Relaxed),
                                    in_now,
                                    out_now,
                                );
                                // Cache tiers: bill cache read/write into
                                // the meter at their discounted rates. Same cumulative
                                // → increment fold as input/output.
                                let cr_now = usage.cache_read_tokens.unwrap_or(0);
                                let cc_now = usage.cache_creation_tokens.unwrap_or(0);
                                let (cr_delta, cc_delta) = token_increment(
                                    last_cache_read.swap(cr_now, Ordering::Relaxed),
                                    last_cache_creation.swap(cc_now, Ordering::Relaxed),
                                    cr_now,
                                    cc_now,
                                );
                                // Anything billed to the meter makes this attempt
                                // terminal — a retry would double-count.
                                if cr_delta > 0 || cc_delta > 0 || in_delta > 0 || out_delta > 0 {
                                    committed.store(true, Ordering::Relaxed);
                                }
                                if cr_delta > 0 || cc_delta > 0 {
                                    meter.add_cache(cr_delta, cc_delta);
                                }
                                if in_delta > 0 || out_delta > 0 {
                                    // Mirror the same increment into the session meter
                                    // (binary-side, survives the TUI) and the panel.
                                    meter.add(in_delta, out_delta);
                                    send_token_delta(user_io_ref, in_delta, out_delta).await;
                                }
                            }
                            event
                        }
                    })
                    .collect_response()
                    .await
            })
            .await;

        let Some(result) = result else {
            live.close(&user_io, "", OUTCOME_ERROR).await;
            return Err(SystemError::ExecutionError("turn interrupted".into()));
        };

        match result {
            Ok(response) => break (response, last_in.load(Ordering::Relaxed)),
            // Only attempts that failed before emitting any token delta are safe
            // to re-run; otherwise the panel tally would double-count.
            Err(err) if attempt < max_attempts && !committed.load(Ordering::Relaxed) => {
                tracing::warn!("act: llm attempt {attempt} failed: {err}; retrying");
            }
            Err(err) => {
                // Keep whatever text reached the user and say plainly that it
                // stops there, rather than leaving a block that looks like it
                // is still being written.
                live.close(&user_io, "", OUTCOME_ERROR).await;
                let _ = user_io
                    .send(report_unavailable(format!(
                        "provider failed: {err}; recovery is required before more work can run"
                    )))
                    .await;
                return Err(SystemError::ExecutionError(err.to_string()));
            }
        }
    };

    tool_palette_decision(&policy.tool_palette, offered, max_attempts, attempt)
        .record(&store, prepared.turn);

    // Feed the provider's real `input_tokens` + the sent window back so the next
    // turn's projection can calibrate its tokens-per-char estimate. Success path
    // only, so a failed/retried attempt never miscalibrates.
    context.record_sent_window(prompt_tokens, &messages);
    status::send_cost(&user_io, &prepared.model_id, &meter).await;

    context.push(Message::Assistant {
        id: None,
        content: response.content.clone(),
    });

    // The model's reasoning and its chosen actions become node-scoped `tracing`
    // events: they nest under this `act` node's span (opened by the progress
    // middleware) and surface under the node in the panel's Ctrl-O timeline,
    // rather than cluttering the transcript. On a terminal turn `finalize`
    // still emits the answer to the transcript, so there's no double-print.
    let reasoning = response.text();
    let tool_calls = response.tool_calls();
    if !reasoning.is_empty() {
        tracing::info!("REASON: {reasoning}");
    }
    for call in tool_calls {
        tracing::info!(
            "Action: {}({})",
            call.function.name,
            call.function.arguments
        );
    }

    Ok(response)
}

/// Execute the response's tool calls, gating by the policy-resolved permission,
/// and reify each call into the trajectory graph.
///
/// Tools run as [`ToolRun`]s — the [`ToolRegistry::to_arc`] + `Tool::execute`
/// pair `ToolRegistry::execute_with` performs inline, in a task of their own —
/// so the two `#[context]` parameters `run_command` already declares are
/// actually supplied: a host sink that streams the command's output into the
/// block the invocation opened, and a cancellation observer adapted from this
/// run's token. The model-visible tool schema is unchanged — `#[context]`
/// values never appear in it.
///
/// The wait is raced against the user's backgrounding request. Losing that
/// race **hands the wait to the host and nothing else**: the run is reserved
/// for the `execute` caller inside `run_command`, and that caller's handle
/// requests a stop when dropped, so the caller must outlive the turn for the
/// command to keep running and for its result to reach anyone. The task keeps
/// it alive; [`BackgroundReporter`](crate::terminal::BackgroundReporter) takes
/// the task over and receives the same `executed` response the model would
/// have. No `start`, `start_confirmed` or `execute` is called here, so nothing
/// is re-run and nothing spawns twice, and the invocation still closes exactly
/// once — with a normal `tool_result` naming the task. An *interrupted* wait,
/// by contrast, is aborted: the tool's future is dropped, as it always was.
///
/// # Lanes
///
/// A batch whose every call only reads and is permitted outright takes the
/// concurrent lane ([`execute_read_only_batch`]); anything else — one mutating
/// call, one confirmation, one denial — keeps the sequential loop below,
/// unchanged. See [`batch_runs_concurrently`] for why the test is that coarse.
#[system]
async fn execute_tools(
    decision: Out<LlmResponse>,
    mut context: ResMut<ContextManager>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
    config: Res<AgentConfig>,
    live: Res<LiveTranscript>,
    cancel: Res<RunCancellation>,
    session: Res<SessionInfo>,
    tasks: Res<BackgroundTasks>,
    redactor: Res<SecretRedactor>,
) -> Result<(), SystemError> {
    let redactor = *redactor;
    let turn = context.turn_marker();
    let mut result_blocks = Vec::new();
    // Files a successful `edit_file` mutated this turn; their Code `ABox` facts
    // (notably symbol spans) are re-indexed after the loop so navigation/reads
    // don't drift against the edited source.
    let mut edited: Vec<std::path::PathBuf> = Vec::new();

    let calls = decision.tool_calls();
    // Resolved once, up front, because the lane decision needs the whole
    // batch's permissions before the first call runs. Hoisting is
    // behaviour-preserving: both inputs — the snapshot and the registry — are
    // read-only for the turn, so resolving early cannot differ from resolving
    // in the loop.
    let permissions: Vec<ToolPermission> = calls
        .iter()
        .map(|call| {
            let registry_default = tool_registry
                .permission(&call.function.name)
                .unwrap_or(ToolPermission::Deny);
            policy
                .permission
                .resolve(&call.function.name, registry_default)
        })
        .collect();

    if batch_runs_concurrently(&calls, &permissions) {
        return execute_read_only_batch(
            &calls,
            &mut context,
            &tool_registry,
            &user_io,
            &policy,
            &store,
            &live,
            &cancel,
            &tasks,
            redactor,
            turn,
        )
        .await;
    }

    for (tool_call, &permission) in calls.iter().zip(&permissions) {
        let name = &tool_call.function.name;

        // One activity block per invocation, keyed by the provider's own call
        // id and nested under the message that asked for it. Every exit below
        // closes it, so a block can never be left spinning.
        let detail = tool_detail(name, &tool_call.function.arguments);
        let activity = ToolActivity::start(
            &user_io,
            &tool_call.id,
            live.current_id(),
            name.clone(),
            &detail,
        )
        .await;
        // Claims the backgrounding slot for as long as this invocation lives.
        // Dropping it releases the claim, so a denial, an interrupt, an error
        // and a normal return all leave the host truthful about what is running
        // without each exit path below having to remember.
        let invocation = tasks.enter(&tool_call.id, name, live.current_id());

        let (denied_reason, chosen) = match permission {
            ToolPermission::Deny => (
                Some(format!("Permission denied: tool '{name}' is not allowed")),
                "deny",
            ),
            ToolPermission::Confirm => {
                let prompt = build_confirm_prompt(name, &tool_call.function.arguments);
                // Waiting on the user is not the tool running: say so, so the
                // elapsed time on the block isn't read as the tool being slow.
                activity.phase(&user_io, PHASE_PENDING, &detail).await;
                // An unapproved command must not reach the background by the
                // back door, so the slot says the block is parked and
                // `BackgroundTasks::request` refuses while it is.
                invocation.awaiting_approval();
                // Reify the permission decision (its outcome is the user's answer).
                let answer = match user_io.confirm(prompt).await {
                    Ok(response) if response.confirmed => None,
                    Ok(_) => Some(format!("User denied execution of tool '{name}'")),
                    Err(err) => Some(format!("Confirmation failed: {err}")),
                };
                if answer.is_none() {
                    invocation.executing();
                    activity.phase(&user_io, PHASE_START, &detail).await;
                }
                let chosen = if answer.is_none() { "allow" } else { "deny" };
                (answer, chosen)
            }
            ToolPermission::Allow => (None, "allow"),
        };
        permission_decision(&policy.permission, permission, chosen).record(&store, turn);

        let block = if let Some(reason) = denied_reason {
            activity.finish(&user_io, PHASE_DENIED, &reason).await;
            record_tool_call(
                &store,
                turn,
                name,
                &tool_call.function.arguments,
                "denied",
                &reason,
            );
            UserBlock::tool_error(&tool_call.id, ToolResultContent::Text(reason))
        } else {
            // A run the model deliberately detached is not holding the turn,
            // so it does not inherit the deadline that exists to stop a turn
            // hanging. The tool applies the same fallback to the request; this
            // keeps the table's view of the deadline equal to the real one.
            let deadline = if name == RUN_IN_BACKGROUND {
                tasks.background_deadline_for(&tool_call.function.arguments)
            } else {
                tasks.deadline_for(&tool_call.function.arguments)
            };
            let sink = invocation.sink(&session.session_id, session.turn_number, deadline);
            // The turn's token reaches this command, and stops reaching it the
            // moment the user backgrounds it: a detached run is no longer part
            // of the turn, so Ctrl-C must not take it with the turn.
            let (detached_tx, detached_rx) = tokio::sync::watch::channel(false);
            let tool_context = ToolContext::new()
                .with(ShellSinkHandle::new(sink))
                .with(detachable_cancel(&cancel, detached_rx));
            let mut run = ToolRun::spawn(
                &tool_registry,
                name,
                tool_call.function.arguments.clone(),
                tool_context,
            );
            let outcome = cancel
                .run_until_cancelled(async {
                    loop {
                        tokio::select! {
                            value = &mut run => break Some(value),
                            () = tokio::time::sleep(COALESCE_TICK) => {
                                if invocation.take_request() {
                                    break None;
                                }
                                // At most one message per tick, carrying the
                                // newest line — never one per chunk.
                                if let Some(line) = tasks.take_update(&tool_call.id) {
                                    // Live stdout reaches the terminal ahead
                                    // of the result, so it is redacted here
                                    // too — best effort, because a secret
                                    // split across two chunks is two lines by
                                    // the time either is seen.
                                    let line = redactor.scan(&line).into_text();
                                    activity.phase(&user_io, PHASE_RUNNING, &line).await;
                                }
                            }
                        }
                    }
                })
                .await;
            let Some(outcome) = outcome else {
                // The turn is over, and so is its wait: drop the tool's
                // future, as an interrupted invocation always has. For a
                // command that is the executor's own stop-on-drop on top of
                // the turn token the run was started with.
                run.abort();
                let output = "Interrupted by user".to_string();
                activity.finish(&user_io, PHASE_ERROR, &output).await;
                record_tool_call(
                    &store,
                    turn,
                    name,
                    &tool_call.function.arguments,
                    "interrupted",
                    &output,
                );
                result_blocks.push(UserBlock::tool_error(
                    &tool_call.id,
                    ToolResultContent::Text(output),
                ));
                context.push(Message::User {
                    content: result_blocks,
                });
                return Err(SystemError::ExecutionError("turn interrupted".into()));
            };
            // The user pushed this command out of the turn. Say so on the
            // block, close the invocation once with an ordinary result naming
            // the task, and hand the wait to the host's reporter — which is
            // the only thing that will ever wait on it again.
            let Some(result) = outcome else {
                // Latch the detachment before the sender goes out of scope, so
                // the observer this run was started with stops relaying the
                // turn's token: the user backgrounded it precisely so that
                // Ctrl-C on the turn would leave it alone.
                let _ = detached_tx.send(true);
                let task = match invocation.background(run) {
                    Ok(task) => task,
                    Err(run) => {
                        // Nothing tracks the run, so nothing would report it
                        // or let the user stop it by number. The wait was
                        // never handed over, so it is ended the way an
                        // interrupted one is rather than left as an orphan.
                        run.abort();
                        let reason = "the command could not be backgrounded because the \
                                      host lost track of it, so it was stopped rather than \
                                      left running unwatched"
                            .to_string();
                        activity.finish(&user_io, PHASE_ERROR, &reason).await;
                        record_tool_call(
                            &store,
                            turn,
                            name,
                            &tool_call.function.arguments,
                            "error",
                            &reason,
                        );
                        result_blocks.push(UserBlock::tool_error(
                            &tool_call.id,
                            ToolResultContent::Text(reason),
                        ));
                        continue;
                    }
                };
                let body = background_body(&task);
                activity.finish(&user_io, PHASE_BACKGROUND, &body).await;
                record_tool_call(
                    &store,
                    turn,
                    name,
                    &tool_call.function.arguments,
                    "backgrounded",
                    &body,
                );
                tasks.report_backgrounded(task.number);
                result_blocks.push(UserBlock::tool_result(
                    &tool_call.id,
                    ToolResultContent::Text(background_receipt(&task)),
                ));
                continue;
            };
            // The run's task answered. A wait that ended without an answer —
            // the tool panicked — is reported as the error it is rather than
            // taking the turn down with it.
            let result = result.unwrap_or_else(|err| {
                Err(ToolError::execution_error(format!(
                    "the tool's task ended without a result: {err}"
                )))
            });
            match result {
                Ok(value) => {
                    if let Some(reason) = unattended_shell_refusal(name, &value, &policy.permission)
                    {
                        activity.finish(&user_io, PHASE_DENIED, &reason).await;
                        record_tool_call(
                            &store,
                            turn,
                            name,
                            &tool_call.function.arguments,
                            "denied",
                            &reason,
                        );
                        result_blocks.push(UserBlock::tool_error(
                            &tool_call.id,
                            ToolResultContent::Text(reason),
                        ));
                        continue;
                    }
                    // Every toolset in this crate answers with the string it
                    // built; `run_command` answers with the executor's JSON
                    // document. The document is what the trajectory and the
                    // host block keep — it is the honest record — but what
                    // the model gets is that document rendered as the text a
                    // shell produced, with an exit marker it cannot miss.
                    let observed = value
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| value.to_string());
                    // The one seam, applied to both strings: everything
                    // downstream — the log line below, the terminal block, the
                    // reified `ToolCall` in the Session `ABox`, and the
                    // provider-bound `UserBlock` — is built from these two and
                    // nothing else. The rendering is scanned in its own right
                    // because it is re-read from the raw response, not from
                    // the scanned text.
                    let observed = redact_tool_output(redactor, name, observed);
                    let output = (name == "run_command")
                        .then(|| render_run_command(&value))
                        .flatten()
                        .map(|rendered| redact_tool_output(redactor, name, rendered))
                        .unwrap_or_else(|| observed.clone());
                    tracing::info!("Observation: {observed}");
                    activity.finish(&user_io, PHASE_OK, &observed).await;
                    record_tool_call(
                        &store,
                        turn,
                        name,
                        &tool_call.function.arguments,
                        "ok",
                        &observed,
                    );
                    if name == "edit_file"
                        && let Some(rel) = tool_call
                            .function
                            .arguments
                            .get("path")
                            .and_then(|v| v.as_str())
                    {
                        edited.push(config.working_dir.join(rel));
                    }
                    UserBlock::tool_result(&tool_call.id, ToolResultContent::Text(output))
                }
                Err(err) => {
                    // A failure message carries tool output too — a command's
                    // stderr, a parser's echo of the line it choked on.
                    let output = redact_tool_output(redactor, name, err.to_string());
                    activity.finish(&user_io, PHASE_ERROR, &output).await;
                    record_tool_call(
                        &store,
                        turn,
                        name,
                        &tool_call.function.arguments,
                        "error",
                        &output,
                    );
                    UserBlock::tool_error(&tool_call.id, ToolResultContent::Text(output))
                }
            }
        };
        result_blocks.push(block);
    }

    context.push(Message::User {
        content: result_blocks,
    });

    // Keep the Code `ABox` fresh: incrementally re-index every file an
    // `edit_file` mutated this turn, so spans don't drift after a write. Best
    // effort — a stale graph degrades navigation but must not fail the turn.
    if !edited.is_empty() {
        match CodeIndexer::index_paths(&store, &config.working_dir, &edited).await {
            Ok(n) => {
                tracing::debug!("code graph: re-indexed {} file(s), {n} facts", edited.len());
                // Re-derive the conceptual-role layer so concept queries track
                // the edited source too. This rebuilds the whole enrichment graph
                // each edit; scoping it to re-indexed symbols is deferred.
                if let Err(e) = enrich(&store) {
                    tracing::warn!("code graph: enrichment after edit failed: {e}");
                }
            }
            Err(e) => tracing::warn!("code graph: incremental re-index failed: {e}"),
        }
    }

    Ok(())
}

/// Whether this batch may be run concurrently rather than one call at a time.
///
/// Three conditions, and the batch is judged as a whole rather than sliced into
/// runs:
///
/// 1. **More than one call.** A single call is the sequential loop's own case;
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
/// The invocations run with a bare [`ToolContext`]. The only `#[context]`
/// values this host supplies are the shell sink and the detachable cancellation
/// observer, and the only tool that declares them is `run_command`, which is
/// not read-only and therefore never reaches this lane.
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
    policy: &PolicySnapshot,
    store: &OxigraphStore,
    live: &LiveTranscript,
    cancel: &RunCancellation,
    tasks: &BackgroundTasks,
    redactor: SecretRedactor,
    turn: usize,
) -> Result<(), SystemError> {
    // Every call in this batch resolved to `Allow`; reify that in request order
    // before anything runs, so the trajectory reads identically whichever lane
    // executed the batch.
    for _ in calls {
        permission_decision(&policy.permission, ToolPermission::Allow, "allow").record(store, turn);
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

    let tool_context = ToolContext::new();
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
                UserBlock::tool_result(&call.id, ToolResultContent::Text(output))
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
                UserBlock::tool_error(&call.id, ToolResultContent::Text(output))
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
                UserBlock::tool_error(&call.id, ToolResultContent::Text(output))
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
        && permission.fail_closed_on_ambiguous_shell_permission
        && value.get("status").and_then(serde_json::Value::as_str) == Some("confirmation_required"))
    .then(|| "Permission denied: shell command is outside the unattended grant".to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// Secret redaction
// ─────────────────────────────────────────────────────────────────────────────

/// How a withheld value is spelled in the text every consumer sees.
///
/// Deliberately not a plausible value: a developer who reads
/// `[REDACTED:anthropic-api-key]` in the transcript cannot mistake it for the
/// key that was there, and it greps.
const REDACTION_MARKER: &str = "[REDACTED:";

/// The character class a key's body is drawn from, after its literal prefix.
#[derive(Clone, Copy)]
enum KeyBody {
    /// `A-Z a-z 0-9 _ -`
    Base64Url,
    /// `A-Z a-z 0-9`
    Alnum,
    /// `A-Z 0-9`
    UpperAlnum,
    /// `A-Z a-z 0-9 _`
    AlnumUnderscore,
    /// `A-Z a-z 0-9 -`
    AlnumDash,
}

impl KeyBody {
    /// Whether `byte` continues a key body of this class.
    const fn accepts(self, byte: u8) -> bool {
        match self {
            Self::Base64Url => byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-',
            Self::Alnum => byte.is_ascii_alphanumeric(),
            Self::UpperAlnum => byte.is_ascii_uppercase() || byte.is_ascii_digit(),
            Self::AlnumUnderscore => byte.is_ascii_alphanumeric() || byte == b'_',
            Self::AlnumDash => byte.is_ascii_alphanumeric() || byte == b'-',
        }
    }
}

/// One recognisable credential shape: a literal prefix plus a bounded body.
struct KeyShape {
    /// The name that appears inside the marker.
    kind: &'static str,
    /// Literal prefixes that introduce this shape.
    prefixes: &'static [&'static str],
    /// Which bytes continue the body.
    body: KeyBody,
    /// Inclusive body-length bounds. A run outside them is not this shape.
    min: usize,
    /// Inclusive upper bound; see [`KeyShape::min`].
    max: usize,
    /// Whether the body must contain at least one digit.
    ///
    /// Only set on prefixes generic enough to collide with ordinary
    /// kebab-case text (`sk-`, `xoxb-`). This is not entropy scoring — it is
    /// one bit that every issued key of these shapes carries and English
    /// identifiers do not.
    needs_digit: bool,
}

/// Every credential shape recognised by literal prefix, most specific first.
///
/// Deliberately closed and deliberately short. What is *not* here is as much
/// of the contract as what is — see `docs/secret-redaction.md`.
const KEY_SHAPES: &[KeyShape] = &[
    KeyShape {
        kind: "anthropic-api-key",
        prefixes: &["sk-ant-"],
        body: KeyBody::Base64Url,
        min: 24,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "openai-api-key",
        prefixes: &["sk-proj-"],
        body: KeyBody::Base64Url,
        min: 24,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "openai-api-key",
        prefixes: &["sk-"],
        body: KeyBody::Base64Url,
        min: 32,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "aws-access-key-id",
        prefixes: &[
            "AKIA", "ASIA", "AROA", "AIDA", "AGPA", "AIPA", "ANPA", "ANVA", "ABIA",
        ],
        body: KeyBody::UpperAlnum,
        min: 16,
        max: 16,
        needs_digit: false,
    },
    KeyShape {
        kind: "google-api-key",
        prefixes: &["AIza"],
        body: KeyBody::Base64Url,
        min: 35,
        max: 35,
        needs_digit: false,
    },
    KeyShape {
        kind: "github-token",
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        body: KeyBody::Alnum,
        min: 36,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "github-token",
        prefixes: &["github_pat_"],
        body: KeyBody::AlnumUnderscore,
        min: 40,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "gitlab-token",
        prefixes: &["glpat-"],
        body: KeyBody::Base64Url,
        min: 20,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "slack-token",
        prefixes: &["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-", "xoxe-"],
        body: KeyBody::AlnumDash,
        min: 10,
        max: 255,
        needs_digit: true,
    },
    KeyShape {
        kind: "stripe-secret-key",
        prefixes: &["sk_live_", "rk_live_"],
        body: KeyBody::AlnumUnderscore,
        min: 16,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "npm-token",
        prefixes: &["npm_"],
        body: KeyBody::Alnum,
        min: 36,
        max: 36,
        needs_digit: false,
    },
    KeyShape {
        kind: "hugging-face-token",
        prefixes: &["hf_"],
        body: KeyBody::Alnum,
        min: 30,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "pypi-token",
        prefixes: &["pypi-AgEIcHlwaS5vcmc"],
        body: KeyBody::Base64Url,
        min: 16,
        max: 4096,
        needs_digit: false,
    },
];

/// Header names whose whole value is a credential.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "x-amz-security-token",
];

/// Authentication schemes kept in front of a redacted header value, so the
/// reader still knows *what kind* of credential was withheld.
const AUTH_SCHEMES: &[&str] = &["Bearer", "Basic", "Digest", "Token", "Negotiate"];

/// Name words that mark the value bound to them as a secret.
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "credential",
    "credentials",
    "passphrase",
    "apikey",
];

/// Two-word name endings that mark a secret where neither word does alone.
///
/// `key` on its own is far too common (`sort_key`, `cache_key`, `public_key`)
/// to be a rule.
const SECRET_WORD_PAIRS: &[&str] = &[
    "api key",
    "access key",
    "secret key",
    "private key",
    "signing key",
    "encryption key",
    "auth token",
];

/// The result of scanning one string for secrets.
///
/// Carries the rewritten text plus what was recognised, so a caller can log
/// *that* a substitution happened without logging the value.
#[derive(Debug, Clone)]
pub struct Redaction {
    /// The text with every recognised value replaced by a marker.
    text: String,
    /// Which shapes fired, in first-seen order, deduplicated.
    kinds: Vec<&'static str>,
    /// How many substitutions were made.
    count: usize,
}

impl Redaction {
    /// The rewritten text. Identical to the input when nothing was recognised.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the redaction, yielding the rewritten text.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// Whether nothing was recognised and the text is unchanged.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.count == 0
    }

    /// How many values were substituted.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Which shapes fired, in first-seen order.
    #[must_use]
    pub fn kinds(&self) -> &[&'static str] {
        &self.kinds
    }

    /// A one-line notice naming what was withheld, or `None` when nothing was.
    ///
    /// Appended to a tool result so the model is told the bytes it is reading
    /// are not the bytes on disk, rather than left to infer it from a marker.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        (!self.is_clean()).then(|| {
            format!(
                "\n[polaris-code redacted {} secret-shaped value(s) from this result: {}]",
                self.count,
                self.kinds.join(", ")
            )
        })
    }
}

/// Replaces recognisable credentials in tool output with a visible marker,
/// before that output reaches the model, the transcript, the log or the
/// session graph.
///
/// # Scope
///
/// This is an **accident** control, not a permission boundary. It reduces what
/// an agent doing ordinary work — reading a `.env`, printing an environment,
/// tailing a log — leaks to a provider and to a session file on disk. It does
/// not stop a model that is trying to read a secret out, because the model can
/// always ask a tool to transform the bytes first (base64, reverse, print one
/// character per line). Never describe it as containment; see
/// `docs/secret-redaction.md` for what is and is not covered.
///
/// # Scope (resource)
///
/// Global. The rule set is process configuration chosen once at the
/// composition root, not per-session state.
///
/// # Provider
///
/// [`CodeAgentPlugin`], from [`CodeAgentPlugin::with_redaction`]. The binary
/// gates it on `POLARIS_CODE_REDACT`; every other host gets it enabled.
///
/// # Consuming system
///
/// [`execute_tools`] reads it as `Res<SecretRedactor>` and applies it to each
/// tool result — the one seam every consumer of that result is downstream of.
///
/// # Example
///
/// ```
/// use polaris_code::agents::code_agent::SecretRedactor;
///
/// let redactor = SecretRedactor::new(true);
/// let scan = redactor.scan("ANTHROPIC_API_KEY=sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm");
/// assert!(!scan.is_clean());
/// assert!(scan.text().contains("[REDACTED:anthropic-api-key]"));
///
/// // Ordinary code is left alone.
/// let clean = redactor.scan("let token = lexer.next_token();");
/// assert!(clean.is_clean());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretRedactor {
    /// Whether scanning is on. Off returns every input unchanged.
    enabled: bool,
}

impl Default for SecretRedactor {
    fn default() -> Self {
        Self::new(true)
    }
}

impl GlobalResource for SecretRedactor {}

impl SecretRedactor {
    /// A redactor that is on when `enabled`, and a pass-through when not.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Whether this redactor rewrites anything at all.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Scans `input` and returns the rewritten text plus what was recognised.
    #[must_use]
    pub fn scan(self, input: &str) -> Redaction {
        if !self.enabled {
            return Redaction {
                text: input.to_string(),
                kinds: Vec::new(),
                count: 0,
            };
        }
        let mut hits = Hits::default();
        let without_keys = redact_key_blocks(input, &mut hits);
        let mut text = String::with_capacity(without_keys.len());
        // `split_inclusive` keeps each terminator on its own line, so the
        // rewritten lines concatenate back to the original byte for byte.
        for line in without_keys.split_inclusive('\n') {
            redact_line(line, &mut text, &mut hits);
        }
        Redaction {
            text,
            kinds: hits.kinds,
            count: hits.count,
        }
    }
}

/// What a scan recognised so far.
#[derive(Default)]
struct Hits {
    /// Shapes seen, first-seen order, deduplicated.
    kinds: Vec<&'static str>,
    /// Total substitutions.
    count: usize,
}

impl Hits {
    /// Records one substitution of `kind` and returns its marker.
    fn mark(&mut self, kind: &'static str) -> String {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
        self.count += 1;
        format!("{REDACTION_MARKER}{kind}]")
    }
}

/// Replaces whole PEM private-key blocks, which are the one shape that spans
/// lines — and the one worth catching even when the output is truncated, so an
/// unterminated block is redacted to the end of the input.
///
/// Newline-agnostic on purpose: a key inside a JSON-encoded shell result has
/// its line breaks escaped, and is still a key.
fn redact_key_blocks(input: &str, hits: &mut Hits) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const TAIL: &str = "PRIVATE KEY-----";
    /// How far past `-----BEGIN ` the `PRIVATE KEY-----` tail may sit.
    const LABEL_BUDGET: usize = 48;

    if !input.contains(TAIL) {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(BEGIN) {
        let label_end = (start + BEGIN.len() + LABEL_BUDGET).min(rest.len());
        let Some(tail) = rest[start..label_end].find(TAIL) else {
            // Some other PEM object (a certificate, a public key). Copy the
            // marker itself and keep looking after it.
            out.push_str(&rest[..start + BEGIN.len()]);
            rest = &rest[start + BEGIN.len()..];
            continue;
        };
        let after_header = start + tail + TAIL.len();
        let end = rest[after_header..]
            .find(END)
            .and_then(|offset| {
                let from = after_header + offset;
                let budget = (from + END.len() + LABEL_BUDGET).min(rest.len());
                rest[from..budget]
                    .find(TAIL)
                    .map(|tail| from + tail + TAIL.len())
            })
            .unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        out.push_str(&hits.mark("private-key"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Rewrites one line (terminator included) into `out`.
fn redact_line(line: &str, out: &mut String, hits: &mut Hits) {
    if let Some((value_start, value_end)) = credential_header(line) {
        out.push_str(&line[..value_start]);
        out.push_str(&hits.mark("authorization-header"));
        out.push_str(&line[value_end..]);
        return;
    }

    let bytes = line.as_bytes();
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if !line.is_char_boundary(index) {
            index += 1;
            continue;
        }
        if let Some((start, end)) = url_password(line, index) {
            out.push_str(&line[copied..start]);
            out.push_str(&hits.mark("url-credentials"));
            index = end;
            copied = end;
            continue;
        }
        if let Some((end, kind)) = key_shape_at(line, index) {
            out.push_str(&line[copied..index]);
            out.push_str(&hits.mark(kind));
            index = end;
            copied = end;
            continue;
        }
        if (bytes[index] == b'=' || bytes[index] == b':')
            && let Some((start, end)) = secret_assignment(line, index)
        {
            // When the value is *also* a recognisable shape, say which — the
            // name told us it was a secret, the shape tells the reader what
            // kind of secret it was.
            let kind = key_shape_at(line, start)
                .filter(|(key_end, _)| *key_end == end)
                .map_or("secret-assignment", |(_, kind)| kind);
            out.push_str(&line[copied..start]);
            out.push_str(&hits.mark(kind));
            index = end;
            copied = end;
            continue;
        }
        index += 1;
    }
    out.push_str(&line[copied..]);
}

/// Whether a key shape may start at `index`: the byte before it must not be
/// part of a longer identifier, or `disk-…` would look like a key.
fn at_token_start(line: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = line.as_bytes()[index - 1];
    !(previous.is_ascii_alphanumeric() || previous == b'_' || previous == b'-' || previous == b'.')
}

/// Matches any [`KEY_SHAPES`] entry starting at `index`, returning the end of
/// the match and the shape's name.
fn key_shape_at(line: &str, index: usize) -> Option<(usize, &'static str)> {
    if !at_token_start(line, index) {
        return None;
    }
    let rest = &line[index..];
    let bytes = line.as_bytes();
    for shape in KEY_SHAPES {
        for prefix in shape.prefixes {
            if !rest.starts_with(prefix) {
                continue;
            }
            let body_start = index + prefix.len();
            let mut end = body_start;
            while end < bytes.len() && shape.body.accepts(bytes[end]) {
                end += 1;
            }
            let length = end - body_start;
            if length < shape.min || length > shape.max {
                continue;
            }
            if shape.needs_digit && !bytes[body_start..end].iter().any(u8::is_ascii_digit) {
                continue;
            }
            return Some((end, shape.kind));
        }
    }
    json_web_token_at(line, index)
}

/// Matches a signed JSON Web Token — three dot-separated base64url segments
/// whose header begins `eyJ`, which is `{"` encoded.
fn json_web_token_at(line: &str, index: usize) -> Option<(usize, &'static str)> {
    /// Shortest a segment may be before this stops looking like a token.
    const SEGMENT: usize = 10;

    if !line[index..].starts_with("eyJ") {
        return None;
    }
    let bytes = line.as_bytes();
    let mut end = index;
    for segment in 0..3 {
        if segment > 0 {
            if end >= bytes.len() || bytes[end] != b'.' {
                return None;
            }
            end += 1;
        }
        let start = end;
        while end < bytes.len() && KeyBody::Base64Url.accepts(bytes[end]) {
            end += 1;
        }
        if end - start < SEGMENT {
            return None;
        }
    }
    Some((end, "jwt"))
}

/// Finds the password in a `scheme://user:password@host` authority starting at
/// `index`, returning its byte range.
fn url_password(line: &str, index: usize) -> Option<(usize, usize)> {
    if !line[index..].starts_with("://") {
        return None;
    }
    let authority_start = index + 3;
    let bytes = line.as_bytes();
    let mut cursor = authority_start;
    let mut colon = None;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'@' => break,
            b':' if colon.is_none() => colon = Some(cursor),
            b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' => return None,
            byte if byte.is_ascii_whitespace() => return None,
            _ => {}
        }
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    let colon = colon?;
    (cursor > colon + 1).then_some((colon + 1, cursor))
}

/// Finds the value of a credential-bearing header on `line`, returning the
/// byte range to withhold — the scheme word, when there is one, is kept.
fn credential_header(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut start = 0;
    // Skip the decoration a header arrives wrapped in: indentation, curl's
    // `>`/`<`/`*` trace prefixes, and a `-H "` / `--header '` argument.
    loop {
        let before = start;
        while start < bytes.len() && (bytes[start] as char).is_ascii_whitespace() {
            start += 1;
        }
        while start < bytes.len() && matches!(bytes[start], b'>' | b'<' | b'*' | b'"' | b'\'') {
            start += 1;
        }
        for flag in ["--header", "-H"] {
            if line[start..].starts_with(flag) {
                start += flag.len();
                break;
            }
        }
        if start == before {
            break;
        }
    }
    let colon = line[start..].find(':')? + start;
    let name = line[start..colon].trim().to_ascii_lowercase();
    if !CREDENTIAL_HEADERS.contains(&name.as_str()) {
        return None;
    }
    let mut value_start = colon + 1;
    while value_start < bytes.len() && matches!(bytes[value_start], b' ' | b'\t') {
        value_start += 1;
    }
    let value_end = line[value_start..]
        .trim_end()
        .trim_end_matches(['"', '\''])
        .len()
        + value_start;
    if value_end <= value_start {
        return None;
    }
    // Keep the scheme so the reader knows what kind of credential it was.
    for scheme in AUTH_SCHEMES {
        let prefixed = format!("{scheme} ");
        if line[value_start..value_end]
            .to_ascii_lowercase()
            .starts_with(&prefixed.to_ascii_lowercase())
        {
            let after = value_start + prefixed.len();
            return (after < value_end).then_some((after, value_end));
        }
    }
    (!is_placeholder(&line[value_start..value_end])).then_some((value_start, value_end))
}

/// Finds a `name = value` / `name: value` binding whose *name* marks the value
/// as a secret, with the separator at `index`, returning the value's range.
fn secret_assignment(line: &str, index: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut back = index;
    while back > 0 && matches!(bytes[back - 1], b' ' | b'\t') {
        back -= 1;
    }
    if back > 0 && matches!(bytes[back - 1], b'"' | b'\'') {
        back -= 1;
    }
    let name_end = back;
    while back > 0 && is_name_byte(bytes[back - 1]) {
        back -= 1;
    }
    let name = &line[back..name_end];
    if !is_secret_name(name) {
        return None;
    }
    // An all-caps name is the environment-variable shape: on its own it is
    // strong enough evidence to accept a bare, short value. A lower-case name
    // is also an ordinary identifier, so it has to clear a higher bar.
    let shouted = !name.bytes().any(|byte| byte.is_ascii_lowercase());

    let mut start = index + 1;
    while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
        start += 1;
    }
    if start >= bytes.len() {
        return None;
    }
    if matches!(bytes[start], b'"' | b'\'') {
        let quote = bytes[start];
        let open = start + 1;
        let mut end = open;
        while end < bytes.len() && bytes[end] != quote {
            end += usize::from(bytes[end] == b'\\') + 1;
        }
        let end = end.min(bytes.len());
        let value = line.get(open..end)?;
        let acceptable = if shouted {
            value.len() >= 4
        } else {
            value.len() >= 8 && !value.contains(char::is_whitespace)
        };
        (acceptable && !is_placeholder(value)).then_some((open, end))
    } else {
        let mut end = start;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        let value = line.get(start..end)?;
        // A bare value under a lower-case name is the weakest signal there is
        // — `password: Option<String>` is a struct field, not a credential —
        // so it has to be long and carry a digit before it counts.
        let acceptable = if shouted {
            value.len() >= 4
        } else {
            value.len() >= 16 && value.bytes().any(|byte| byte.is_ascii_digit())
        };
        (acceptable && !is_placeholder(value)).then_some((start, end))
    }
}

/// Whether `byte` can appear in a binding name.
const fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
}

/// Values that are shaped like secrets but cannot be one: a template
/// reference, a flag, a documentation stand-in, an already-masked value.
fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('$')
        || value.starts_with('<')
        || value.starts_with("{{")
        || value.starts_with("%(")
    {
        return true;
    }
    if value
        .chars()
        .all(|character| matches!(character, '*' | 'x' | 'X' | '.' | '…' | '-'))
    {
        return true;
    }
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "0" | "1" | "yes" | "no" | "on" | "off" | "null" | "none" | "nil"
    )
}

/// Whether a binding name marks its value as a secret.
///
/// Matched on the name's *last* word (and, for the `… key` endings that need
/// it, its last two), never on a substring: `secret_name` and `sort_key` are
/// ordinary identifiers, `client_secret` and `api_key` are not.
fn is_secret_name(name: &str) -> bool {
    let words = name_words(name);
    let Some(last) = words.last() else {
        return false;
    };
    // `tokenizer`, `tokenize`, `tokenization` — a lexer, not a credential.
    if words.iter().any(|word| word.starts_with("tokeniz")) {
        return false;
    }
    if SECRET_WORDS.contains(&last.as_str()) {
        return true;
    }
    words.len() >= 2 && {
        let pair = format!("{} {last}", words[words.len() - 2]);
        SECRET_WORD_PAIRS.contains(&pair.as_str())
    }
}

/// Splits a binding name into lower-case words on separators and `camelCase`
/// boundaries: `ANTHROPIC_API_KEY` and `anthropicApiKey` both give
/// `["anthropic", "api", "key"]`.
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if matches!(character, '_' | '-' | '.' | ' ') {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Applies `redactor` to one tool result, announcing the substitution to the
/// reader and to the log without putting the value in either.
///
/// This is the single seam: `execute_tools` calls it once, and the redacted
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
/// [`ShellExecutor::stop`](polaris::shell::ShellExecutor::stop) on that process
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
    user_io: Res<UserIO>,
    store: Res<OxigraphStore>,
    mut live: ResMut<LiveTranscript>,
) -> LoopState {
    let text = decision.text();
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

/// Recover from a caught error: surface it, add it to context, respond gracefully.
#[system]
async fn recover(
    error: ErrOut<CaughtError>,
    mut context: ResMut<ContextManager>,
    llm: Res<AgentLlm>,
    meter: Res<TokenMeter>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    project: Res<ProjectContext>,
    mut live: ResMut<LiveTranscript>,
    mut loop_state: ResMut<LoopState>,
) -> LoopState {
    // Whatever the failed system left open stops here. Its partial text stays
    // on screen, marked as ended in error rather than still arriving.
    live.close(&user_io, "", OUTCOME_ERROR).await;
    let error_text = format!(
        "System '{}' failed: {} (node {}, {:?}, kind {})",
        error.system_name, error.message, error.node_id, error.duration, error.kind
    );
    send_error(&user_io, format!("\n[System Error] {error_text}")).await;
    context.push(Message::user(format!("[System Error] {error_text}")));

    // Keep the same project grounding the loop's `prepare_context` injects, so a
    // recovery turn doesn't silently lose git/project context.
    let system_prompt = format!("{SYSTEM_PROMPT}\n\n{}", project.system_prompt());
    let messages = context.messages.clone();
    let builder = llm
        .builder()
        .system(system_prompt)
        .messages(messages)
        .with_registry(&tool_registry)
        .cache_prefix();

    meter.record_model_turn();
    let failure = match builder.generate().await {
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
            send_error(&user_io, format!("\nLLM error: {err}")).await;
            let _ = user_io
                .send(report_unavailable(format!(
                    "recovery provider failed: {err}; restart or resume the session"
                )))
                .await;
            live.whole(
                &user_io,
                "I encountered an error processing your request.",
                OUTCOME_ERROR,
            )
            .await;
            // The recovery call itself also failed, so there is no assistant
            // answer for this turn. Carry the original failure forward so the
            // turn is reported as failed instead of completed — otherwise the
            // graph reaching its terminal `is_complete` state here reads as
            // success even though nothing answered the user.
            Some(error_text.clone())
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

/// How many times the agent loop may go round in one turn when
/// `POLARIS_CODE_MAX_TURNS` is unset.
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

        // Build per-session project grounding and the policy snapshot. These are
        // synchronous to keep `setup` non-async; ProjectContext shells out, so we
        // block on a short-lived runtime handle.
        let project = futures_block_on(ProjectContext::discover(&config.working_dir));
        ctx.insert(project);
        ctx.insert(PolicySnapshot::defaults());
        ctx.insert(LiveTranscript::default());
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
        // other host gets one that reports to nobody: `execute_tools` reads it
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

        graph.add_loop::<LoopState, _, _>(
            "agent_loop",
            |state| state.is_complete,
            |g| {
                g.add_system(apply_steering);
                g.add_system(prepare_context);
                g.add_system(act);
                g.add_conditional_branch::<LlmResponse, _, _, _>(
                    "has_tool_calls",
                    |response| response.has_tool_calls(),
                    |tool_branch| {
                        tool_branch.add_system(execute_tools);
                    },
                    |done_branch| {
                        done_branch.add_system(finalize);
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
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`ContextManager`] | local | Conversation history (persisted). |
/// | [`LoopState`] | local | Loop termination flag. |
/// | [`PolicySnapshot`] | local | Per-turn decision policy (defaults in v1). |
/// | [`SecretRedactor`] | global | Withholds recognisable credentials from every consumer of a tool result. |
///
/// # Dependencies
///
/// Expects `ModelsPlugin`, a tools plugin, and (for persistence) `PersistencePlugin`
/// to be present on the server; resolves the model and project context in `setup`.
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
    /// (config gate). Disabled falls back to the sliding [`LinearWindow`].
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
        ]
    }

    fn build(&self, server: &mut Server) {
        if self.prompt_cache {
            server.register_local(|| {
                ContextManager::with_strategy(std::sync::Arc::new(CacheAwareWindow))
            });
        } else {
            server.register_local(ContextManager::default);
        }
        server.register_local(LoopState::default);
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
    fn token_increment_forwards_only_the_increase() {
        // Cumulative readings produce the per-step increment.
        assert_eq!(token_increment(0, 0, 10, 4), (10, 4));
        assert_eq!(token_increment(10, 4, 25, 9), (15, 5));
        // A repeated reading adds nothing.
        assert_eq!(token_increment(25, 9, 25, 9), (0, 0));
        // A decreasing/out-of-order reading saturates to zero rather than
        // underflowing (which would corrupt the additive tally).
        assert_eq!(token_increment(25, 9, 20, 9), (0, 0));
    }

    #[test]
    fn tool_palette_decision_reifies_mode_and_offered() {
        use crate::agents::PaletteMode;
        let palette = ToolPalettePolicy {
            mode: PaletteMode::Only(vec!["read_file".into()]),
            denied: vec!["edit_file".into()],
        };
        let decision = tool_palette_decision(&palette, vec!["read_file".into()], 2, 1);
        assert_eq!(decision.kind, "tool_palette");
        // `<mode-label>:<offered-count>` — the reified summary the ABox stores.
        assert_eq!(decision.chosen, "selected:1");
        assert_eq!(decision.options, vec!["read_file".to_string()]);
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.mode=selected".to_string())
        );
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.denied=1".to_string())
        );
    }

    #[test]
    fn tool_palette_decision_reports_empty_none_palette() {
        use crate::agents::PaletteMode;
        // The explicit no-tools turn reifies as `none:0` with no options — the
        // empty palette is recorded, never silent.
        let palette = ToolPalettePolicy {
            mode: PaletteMode::None,
            denied: Vec::new(),
        };
        let decision = tool_palette_decision(&palette, Vec::new(), 2, 1);
        assert_eq!(decision.chosen, "none:0");
        assert!(decision.options.is_empty());
        assert!(
            decision
                .policy_refs
                .contains(&"tool_palette.mode=none".to_string())
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Secret redaction
    // ─────────────────────────────────────────────────────────────────────

    /// Scans with redaction on.
    fn scan(input: &str) -> Redaction {
        SecretRedactor::new(true).scan(input)
    }

    /// Asserts `input` is rewritten, that no byte of `secret` survives, and
    /// that the substitution is attributed to `kind`.
    #[track_caller]
    fn withheld(input: &str, secret: &str, kind: &str) {
        let scan = scan(input);
        assert!(!scan.is_clean(), "nothing was recognised in {input:?}");
        assert!(
            !scan.text().contains(secret),
            "{secret:?} survived in {:?}",
            scan.text()
        );
        assert!(
            scan.text().contains(&format!("[REDACTED:{kind}]")),
            "expected a {kind} marker, got {:?}",
            scan.text()
        );
    }

    /// Asserts `input` comes back byte-identical.
    #[track_caller]
    fn untouched(input: &str) {
        let scan = scan(input);
        assert_eq!(scan.text(), input, "ordinary text was rewritten");
        assert!(
            scan.is_clean(),
            "ordinary text was flagged: {:?}",
            scan.kinds()
        );
    }

    #[test]
    fn provider_key_shapes_are_recognised_by_prefix_and_length() {
        for (input, secret, kind) in [
            (
                "key: sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn",
                "sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn",
                "anthropic-api-key",
            ),
            (
                "curl -u AKIAIOSFODNN7EXAMPLE https://s3",
                "AKIAIOSFODNN7EXAMPLE",
                "aws-access-key-id",
            ),
            (
                "remote set-url https://ghp_0123456789abcdefghijklmnopqrstuvwxyz@github.com",
                "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                "github-token",
            ),
            (
                "GITLAB=glpat-0123456789abcdefghij more",
                "glpat-0123456789abcdefghij",
                "gitlab-token",
            ),
            (
                "slack xoxb-1234567890-abcdefghijk done",
                "xoxb-1234567890-abcdefghijk",
                "slack-token",
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
                "dBjftJeZ4CVPmB92K27uhbUJU1p1r",
                "jwt",
            ),
        ] {
            withheld(input, secret, kind);
        }
    }

    #[test]
    fn a_shape_that_is_too_short_or_too_long_is_not_that_shape() {
        // 15 body characters: an AWS key id is exactly 16.
        untouched("AKIA123456789012 AKIA12345678901234567");
        // `sk-` needs 32 body characters and a digit before it counts.
        untouched("sk-short-and-harmless");
        untouched("--sk-a-very-long-kebab-case-flag-name-with-no-digits");
    }

    #[test]
    fn a_name_that_marks_a_secret_withholds_its_value() {
        withheld(
            "DATABASE_PASSWORD=hunter2-correct-horse",
            "hunter2-correct-horse",
            "secret-assignment",
        );
        withheld(
            "export CLIENT_SECRET=\"a quoted value with spaces\"",
            "a quoted value with spaces",
            "secret-assignment",
        );
        withheld(
            "  \"api_key\": \"0123456789abcdef\",",
            "0123456789abcdef",
            "secret-assignment",
        );
        withheld(
            "let auth_token = \"0123456789abcdef\";",
            "0123456789abcdef",
            "secret-assignment",
        );
        withheld(
            "database.password: 9f3a7c1b4e2d8065a1",
            "9f3a7c1b4e2d8065a1",
            "secret-assignment",
        );
    }

    #[test]
    fn ordinary_source_and_command_output_are_left_alone() {
        for line in [
            "let token = self.lexer.next_token();",
            "let error_token = \"unexpected token\";",
            "const MAX_TOKENS: usize = 4096;",
            "const API_KEY_VAR: &str = \"ANTHROPIC_API_KEY\";",
            "    password: Option<String>,",
            "    public_key: String,",
            "let sort_key = \"created_at\";",
            "let cache_key = format!(\"{a}:{b}\");",
            "tokenizer: \"gpt2-byte-level\",",
            "\"input_tokens\": 1024, \"output_tokens\": 77",
            "-const SECRET_HEADER: &str = \"x-request-id\";",
            "+const SECRET_HEADER: &str = \"x-correlation-id\";",
            "test lexer::disk-usage-reporting-path ... ok",
            "https://github.com/RadiantAILabs/polaris_code/blob/main/README.md",
            "ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris",
            "DATABASE_PASSWORD=${DB_PASSWORD}",
            "PASSWORD=",
            "DEBUG_TOKEN=true",
            "api_key = \"<your-key-here>\"",
            "password: \"****\"",
        ] {
            untouched(line);
        }
    }

    #[test]
    fn a_private_key_block_goes_whole_even_when_it_is_truncated() {
        let block = "-----BEGIN RSA PRIVATE KEY-----\n\
                     MIIEpAIBAAKCAQEA1s9pQ0Q\n\
                     -----END RSA PRIVATE KEY-----\n";
        withheld(
            &format!("before\n{block}after\n"),
            "MIIEpAIBAAKCAQEA",
            "private-key",
        );
        assert!(
            scan(&format!("before\n{block}after\n"))
                .text()
                .contains("after")
        );

        // Output truncated mid-block still must not carry the body.
        withheld(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdj",
            "b3BlbnNzaC1rZXktdj",
            "private-key",
        );

        // A certificate or a *public* key is not a private key.
        untouched("-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIE\n-----END CERTIFICATE-----\n");
    }

    #[test]
    fn a_credential_header_keeps_its_scheme_and_loses_its_value() {
        let scan = scan("Authorization: Bearer abcdef0123456789abcdef");
        assert_eq!(
            scan.text(),
            "Authorization: Bearer [REDACTED:authorization-header]"
        );
        withheld(
            "  -H \"x-api-key: 0123456789abcdef\"",
            "0123456789abcdef",
            "authorization-header",
        );
        untouched("Content-Type: application/json");
        untouched("Authorization: ${TOKEN}");
    }

    #[test]
    fn a_url_loses_its_password_and_keeps_everything_else() {
        let scan = scan("git remote add o https://deploy:s3cr3tpass@git.example.com/x.git");
        assert!(!scan.text().contains("s3cr3tpass"), "{}", scan.text());
        assert!(scan.text().contains("https://deploy:"), "{}", scan.text());
        assert!(
            scan.text().contains("@git.example.com/x.git"),
            "{}",
            scan.text()
        );
        untouched("https://user@github.com/RadiantAILabs/polaris_code.git");
        untouched("see https://example.com:8443/path for details");
    }

    #[test]
    fn multi_byte_text_survives_the_scan() {
        untouched("héllo → wörld — ünïcode ✓ 日本語のテキスト");
        let scan = scan("naïve → PASSWORD=hunter2mots ← après");
        assert!(scan.text().starts_with("naïve → PASSWORD="));
        assert!(scan.text().ends_with(" ← après"));
    }

    #[test]
    fn a_disabled_redactor_is_a_pass_through() {
        let raw = "ANTHROPIC_API_KEY=sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn";
        let scan = SecretRedactor::new(false).scan(raw);
        assert_eq!(scan.text(), raw);
        assert!(scan.is_clean());
        assert!(scan.notice().is_none());
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
}
