//! Tool batches execute one call per graph iteration. Authorization, human
//! input and dispatch have separate nodes; the batch owns only its queue and ledger.
//!
//! A session's shell command patterns ([`AgentConfig::command_globs`]) are
//! enforced in the authorization node, per call, next to the permission rules:
//! a command that matches none of them is refused for that call. They are read
//! from the session's own configuration, so two sessions on one server can hold
//! different patterns. File tools and the shell wrapper resolve paths against
//! the session's folder, which both tool lanes put into the tool context as a
//! [`CallRoot`](crate::tools::CallRoot).

use std::collections::VecDeque;

use super::*;
use crate::agents::human_approval::HumanApproval;
use crate::agents::permission_rules::{ApprovalAnswer, ApprovalReceipt};
use crate::terminal::background::Invocation;

/// Independent of the outer model-turn budget.
const TOOL_CALLS_BOUND: usize = u32::MAX as usize;

#[derive(Default)]
pub(crate) struct ToolBatch {
    calls: VecDeque<(usize, ToolCall)>,
    results: Vec<UserBlock>,
    edited: Vec<std::path::PathBuf>,
    completed: usize,
    interrupted: bool,
    trajectory: Option<crate::trajectory::RunTrajectory>,
}
impl LocalResource for ToolBatch {}

pub(crate) struct StartedCall {
    activity: ToolActivity,
    invocation: Invocation,
    call: ToolCall,
    approval_note: Option<String>,
}

// The slot owns the invocation across graph nodes. Clearing it releases the
// background claim immediately, even while graph outputs remain retained.
#[derive(Default)]
pub(crate) enum CallLifetime {
    #[default]
    Idle,
    Active(Box<StartedCall>),
}
impl LocalResource for CallLifetime {}

impl CallLifetime {
    fn take(&mut self) -> StartedCall {
        match std::mem::take(self) {
            Self::Active(call) => *call,
            Self::Idle => unreachable!("authorization owns the active call"),
        }
    }

    fn active_mut(&mut self) -> &mut StartedCall {
        match self {
            Self::Active(call) => call,
            Self::Idle => unreachable!("authorization starts the call before routing it"),
        }
    }
}

pub(crate) struct CallProgress {
    remaining: usize,
    completed: usize,
}

impl ToolBatch {
    fn progress(&self) -> CallProgress {
        CallProgress {
            remaining: if self.interrupted {
                0
            } else {
                self.calls.len()
            },
            completed: self.completed,
        }
    }
}

enum AuthorizationRoute {
    Allow,
    Deny(String),
    Ask(String),
}

pub(crate) struct AuthorizedCall {
    call: ToolCall,
    request: ApprovalRequest,
    receipt: Option<ApprovalReceipt>,
    route: AuthorizationRoute,
    record: Decision,
    detail: String,
}

pub(crate) enum CallDecision {
    Allow(Option<ApprovalAnswer>),
    Deny(String),
}

impl CallDecision {
    fn note(&self) -> Option<&str> {
        match self {
            Self::Allow(Some(answer)) => answer.note.as_deref(),
            _ => None,
        }
    }

    fn push_result(&self, batch: &mut ToolBatch, block: UserBlock) {
        let block = with_approval_note(block, self.note());
        if let Some(trace) = &batch.trajectory {
            trace.tool_results(std::slice::from_ref(&block));
        }
        batch.results.push(block);
    }

    fn record(&self, call: &AuthorizedCall, store: &OxigraphStore, turn: usize) {
        let mut record = call.record.clone();
        record.chosen = if matches!(self, Self::Allow(_)) {
            "allow"
        } else {
            "deny"
        }
        .into();
        if let Self::Allow(Some(answer)) = self {
            record.policy_refs.push(format!(
                "authorization.scope={}",
                approval_scope_label(answer.scope)
            ));
        }
        record.record(store, turn);
    }
}

fn with_approval_note(mut block: UserBlock, note: Option<&str>) -> UserBlock {
    if let Some(note) = note
        && let UserBlock::ToolResult(result) = &mut block
        && let ToolResultContent::Text(text) = &mut result.content
    {
        text.push_str("\n\nUser note on approval: ");
        text.push_str(note);
    }
    block
}

#[system]
pub(crate) async fn prepare_calls(
    decision: Out<LlmResponse>,
    mut batch: ResMut<ToolBatch>,
    context: Res<ContextManager>,
) -> CallProgress {
    *batch = ToolBatch {
        trajectory: context.trajectory(),
        calls: decision
            .tool_calls()
            .into_iter()
            .cloned()
            .enumerate()
            .collect(),
        ..ToolBatch::default()
    };
    batch.progress()
}

// The read-only lane keeps its existing concurrency and output ordering.
#[system]
pub(crate) async fn dispatch_read_only_batch(
    mut batch: ResMut<ToolBatch>,
    mut context: ResMut<ContextManager>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
    config: Res<AgentConfig>,
    live: Res<LiveTranscript>,
    cancel: Res<RunCancellation>,
    tasks: Res<BackgroundTasks>,
    redactor: Res<SecretRedactor>,
    rules: Res<PermissionRules>,
    tool_policies: Res<ToolPolicyRegistry>,
    grants: Res<SessionGrants>,
    shell_approval: Res<ShellApproval>,
    tool_grants: Res<RunToolGrants>,
    settings: Res<SessionSettings>,
) -> CallProgress {
    let calls: Vec<&ToolCall> = batch.calls.iter().map(|(_, call)| call).collect();
    let mut effective = policy.permission.clone();
    effective.rules = Some((*rules).clone());
    effective.tools = (*tool_policies).clone();
    tool_grants.apply(
        &mut effective,
        calls.iter().map(|call| call.function.name.as_str()),
    );
    let permissions: Vec<_> = calls
        .iter()
        .map(|call| {
            shell_approval
                .resolve(
                    &effective,
                    &PermissionCall {
                        tool: &call.function.name,
                        args: &call.function.arguments,
                        registry_default: tool_registry
                            .permission(&call.function.name)
                            .unwrap_or(ToolPermission::Deny),
                        root: &config.working_dir,
                        generation: settings.approval_generation(),
                        ceiling: settings.permission_ceiling(),
                    },
                    &grants,
                )
                .permission
        })
        .collect();
    if batch_runs_concurrently(&calls, &permissions) {
        let turn = context.turn_marker();
        // The concurrent lane reports every failure as a result. Cancellation
        // is already reflected in its result ledger, so it never escapes here.
        let _ = execute_read_only_batch(
            &calls,
            &mut context,
            &tool_registry,
            &user_io,
            &effective,
            &store,
            &live,
            &cancel,
            &tasks,
            *redactor,
            turn,
            &config.working_dir,
        )
        .await;
        batch.calls.clear();
    }
    batch.progress()
}

/// Whether a shell call falls outside the session's command patterns, and if
/// so why.
///
/// This is where a session's [`AgentConfig::command_globs`] are enforced: when
/// each call is authorized, from the session's own configuration, so sessions
/// on one server hold different lists. A session with no list (`None`) is not
/// narrowed here. Only the two tools that start a command carry one; a command
/// is accepted only when every parsed command matches a pattern. Literal
/// interpreter input is excluded from the shell parse; unsupported constructs
/// fail closed. The execution receipt binds this decision to the exact call.
fn outside_command_globs(
    config: &AgentConfig,
    tool: &str,
    args: &serde_json::Value,
) -> Option<String> {
    let globs = config.command_globs.as_ref()?;
    if tool != "run_command" && tool != RUN_IN_BACKGROUND {
        return None;
    }
    let allowed = args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|command| {
            crate::tools::command_check::checked_commands(command, globs).is_some()
        });
    (!allowed).then(|| {
        format!(
            "the command is outside this session's command patterns. {}",
            super::command_patterns_note(globs)
        )
    })
}

#[system]
pub(crate) async fn authorize_call(
    mut batch: ResMut<ToolBatch>,
    mut lifetime: ResMut<CallLifetime>,
    reviews: Out<ReviewResult>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    config: Res<AgentConfig>,
    live: Res<LiveTranscript>,
    tasks: Res<BackgroundTasks>,
    rules: Res<PermissionRules>,
    tool_policies: Res<ToolPolicyRegistry>,
    grants: Res<SessionGrants>,
    shell_approval: Res<ShellApproval>,
    tool_grants: Res<RunToolGrants>,
    settings: Res<SessionSettings>,
) -> AuthorizedCall {
    let (call_index, tool_call) = batch
        .calls
        .pop_front()
        .expect("the loop checks for pending calls");
    let tool_call = &tool_call;
    let mut effective = policy.permission.clone();
    effective.rules = Some((*rules).clone());
    effective.tools = (*tool_policies).clone();
    tool_grants.apply(
        &mut effective,
        std::iter::once(tool_call.function.name.as_str()),
    );
    let policy = &effective;
    let generation = settings.approval_generation();
    let name = &tool_call.function.name;
    // Resolve afresh so a grant from the preceding call applies immediately.
    let mut authorization = shell_approval.resolve_with_patterns(
        policy,
        &PermissionCall {
            tool: name,
            args: &tool_call.function.arguments,
            registry_default: tool_registry
                .permission(name)
                .unwrap_or(ToolPermission::Deny),
            root: &config.working_dir,
            generation,
            ceiling: settings.permission_ceiling(),
        },
        &grants,
        config.command_globs.as_deref(),
    );
    if let Some(reason) = outside_command_globs(&config, name, &tool_call.function.arguments) {
        authorization.permission = ToolPermission::Deny;
        authorization.source = reason;
        authorization.remember = false;
    }
    let permission = authorization.permission;
    let request = ApprovalRequest::new(
        name,
        &tool_call.function.arguments,
        &config.working_dir,
        generation,
        authorization.remember,
    );
    // Bind before any human wait; only the allow path attaches this receipt.
    let mut receipt = (!policy.is_unattended() || shell_approval.unrestricted())
        .then(|| shell_approval.receipt_for(&request, &rules, &settings))
        .flatten();
    let detail = tool_detail(name, &tool_call.function.arguments);
    let activity = ToolActivity::start(
        &user_io,
        &tool_call.id,
        live.current_id(),
        name.clone(),
        &detail,
    )
    .await;
    let invocation = tasks.enter(&tool_call.id, name, live.current_id());
    *lifetime = CallLifetime::Active(Box::new(StartedCall {
        activity,
        invocation,
        call: tool_call.clone(),
        approval_note: None,
    }));
    let review = policy
        .uses_reviewer()
        .then(|| {
            reviews.matching(
                call_index,
                tool_call,
                &config.working_dir,
                &rules,
                &settings,
                &authorization,
            )
        })
        .flatten();
    if let Some(review) = review {
        receipt = review.receipt.clone();
    }
    if permission == ToolPermission::Allow && config.command_globs.is_some() && receipt.is_none() {
        receipt = shell_approval.receipt_for(&request, &rules, &settings);
    }
    let route = match (permission, review) {
        (ToolPermission::Deny, _) => AuthorizationRoute::Deny(format!(
            "Permission denied: tool '{name}' is not allowed ({})",
            authorization.source
        )),
        (ToolPermission::Allow, _) => AuthorizationRoute::Allow,
        (ToolPermission::Confirm, Some(r)) if r.decision == ReviewDecision::Allow => {
            AuthorizationRoute::Allow
        }
        (ToolPermission::Confirm, Some(r)) if r.decision == ReviewDecision::Deny => {
            AuthorizationRoute::Deny(format!("Permission denied: {}", r.notice()))
        }
        (ToolPermission::Confirm, _) => {
            let mut prompt = format!(
                "{}\nDecision source: {}",
                build_confirm_prompt(name, &tool_call.function.arguments),
                authorization.source
            );
            if policy.uses_reviewer() {
                prompt.push('\n');
                prompt.push_str(&review.map_or_else(|| match authorization.review {
                    ReviewEligibility::HumanOnly => "This call needs a person's approval; automatic review does not apply".into(),
                    ReviewEligibility::Eligible => "No matching review for the current call and policy; human approval required".into(),
                }, |r| r.notice()));
            }
            AuthorizationRoute::Ask(prompt)
        }
    };
    let mut recorded = permission_decision(policy, permission, "pending");
    recorded
        .policy_refs
        .push(format!("authorization.source={}", authorization.source));
    if let Some(review) = review {
        let source = match review.decision {
            ReviewDecision::Allow | ReviewDecision::Deny => "reviewer",
            ReviewDecision::Ask => "human",
        };
        recorded
            .policy_refs
            .push(format!("authorization.decision_source={source}"));
        recorded
            .policy_refs
            .push(format!("authorization.reviewer={}", review.model));
        recorded
            .policy_refs
            .push(format!("authorization.review={}", review.notice()));
    } else if policy.uses_reviewer()
        && permission == ToolPermission::Confirm
        && authorization.review == ReviewEligibility::HumanOnly
    {
        // The reviewer was never consulted, so the audit trail says whose
        // decision this is rather than leaving the field out.
        recorded
            .policy_refs
            .push("authorization.decision_source=human".into());
    }
    AuthorizedCall {
        call: tool_call.clone(),
        request,
        receipt,
        route,
        record: recorded,
        detail,
    }
}

#[system]
pub(crate) async fn settle_authorization(call: Out<AuthorizedCall>) -> CallDecision {
    match &call.route {
        AuthorizationRoute::Allow => CallDecision::Allow(None),
        AuthorizationRoute::Deny(reason) => CallDecision::Deny(reason.clone()),
        AuthorizationRoute::Ask(_) => unreachable!("the graph routes approval to ask_human"),
    }
}

#[system]
pub(crate) async fn ask_human(
    call: Out<AuthorizedCall>,
    approver: Option<Res<HumanApproval>>,
    mut lifetime: ResMut<CallLifetime>,
    user_io: Res<UserIO>,
    grants: Res<SessionGrants>,
    redactor: Res<SecretRedactor>,
    cancel: Res<RunCancellation>,
    store: Res<OxigraphStore>,
    mut context: ResMut<ContextManager>,
    mut batch: ResMut<ToolBatch>,
) -> CallDecision {
    let AuthorizationRoute::Ask(prompt) = &call.route else {
        unreachable!("only asking calls enter ask_human")
    };
    let Some(approver) = approver else {
        let decision = CallDecision::Deny("Permission denied: no approver is registered".into());
        record_human_decision(
            &decision,
            &call,
            &store,
            context.turn_marker(),
            "unavailable",
        );
        return decision;
    };
    let active = lifetime.active_mut();
    let waiting = if call.request.frames_command() {
        "awaiting approval"
    } else {
        &call.detail
    };
    active
        .activity
        .phase(&user_io, PHASE_PENDING, waiting)
        .await;
    active.invocation.awaiting_approval();
    let decision = match cancel
        .run_until_cancelled(approver.ask(&call.request, &user_io, prompt.clone(), &grants))
        .await
    {
        Some(Ok(mut answer)) => {
            answer.note = answer
                .note
                .map(|note| redact_tool_output(*redactor, &call.call.function.name, note));
            active.approval_note = answer.note.clone();
            active.invocation.executing();
            active
                .activity
                .phase(&user_io, PHASE_START, &call.detail)
                .await;
            CallDecision::Allow(Some(answer))
        }
        Some(Err(reason)) => CallDecision::Deny(redact_tool_output(
            *redactor,
            &call.call.function.name,
            reason.to_string(),
        )),
        None => {
            let reason = match approver.cancel(&call.request, &user_io).await {
                Ok(()) => "Approval interrupted by user".into(),
                Err(error) => {
                    format!("Approval interrupted by user; could not resolve prompt: {error}")
                }
            };
            let decision = CallDecision::Deny(reason.clone());
            decision.record(&call, &store, context.turn_marker());
            lifetime
                .take()
                .activity
                .finish(&user_io, PHASE_DENIED, &reason)
                .await;
            record_tool_call(
                &store,
                context.turn_marker(),
                &call.call.function.name,
                &call.call.function.arguments,
                "denied",
                &reason,
            );
            batch
                .results
                .push(call.call.error(ToolResultContent::Text(reason)));
            *lifetime = CallLifetime::Idle;
            batch.interrupted = true;
            finish_pending(
                &mut batch,
                &mut context,
                &user_io,
                &store,
                "not run: turn interrupted",
            )
            .await;
            decision
        }
    };
    record_human_decision(&decision, &call, &store, context.turn_marker(), "human");
    decision
}

fn record_human_decision(
    decision: &CallDecision,
    call: &AuthorizedCall,
    store: &OxigraphStore,
    turn: usize,
    source: &str,
) {
    let mut record = Decision::new("human_approval", "deny").with_policy([
        format!("authorization.decision_source={source}"),
        format!("authorization.tool={}", call.call.function.name),
        format!("authorization.call_id={}", call.call.id),
    ]);
    record.options = vec!["allow".into(), "deny".into()];
    match decision {
        CallDecision::Allow(Some(answer)) => {
            record.chosen = "allow".into();
            record.policy_refs.push(format!(
                "authorization.scope={}",
                approval_scope_label(answer.scope)
            ));
        }
        CallDecision::Allow(None) => record.chosen = "allow".into(),
        CallDecision::Deny(reason) => record
            .policy_refs
            .push(format!("authorization.reason={reason}")),
    }
    if let Some(note) = decision.note() {
        record
            .policy_refs
            .push(format!("authorization.note={note}"));
    }
    record.record(store, turn);
}

#[system]
pub(crate) async fn record_denial(
    call: Out<AuthorizedCall>,
    decision: Out<CallDecision>,
    mut lifetime: ResMut<CallLifetime>,
    user_io: Res<UserIO>,
    store: Res<OxigraphStore>,
    context: Res<ContextManager>,
    mut batch: ResMut<ToolBatch>,
) {
    let CallDecision::Deny(reason) = &*decision else {
        unreachable!("denial branch requires a refusal")
    };
    let tool_call = &call.call;
    decision.record(&call, &store, context.turn_marker());
    let active = lifetime.take();
    active.activity.finish(&user_io, PHASE_DENIED, reason).await;
    record_tool_call(
        &store,
        context.turn_marker(),
        &tool_call.function.name,
        &tool_call.function.arguments,
        "denied",
        reason,
    );
    batch
        .results
        .push(tool_call.error(ToolResultContent::Text(reason.clone())));
}

#[system]
pub(crate) async fn execute_call(
    call: Out<AuthorizedCall>,
    decision: Out<CallDecision>,
    mut lifetime: ResMut<CallLifetime>,
    mut batch: ResMut<ToolBatch>,
    mut context: ResMut<ContextManager>,
    tool_registry: Res<ToolRegistry>,
    user_io: Res<UserIO>,
    policy: Res<PolicySnapshot>,
    store: Res<OxigraphStore>,
    config: Res<AgentConfig>,
    cancel: Res<RunCancellation>,
    session: Res<SessionInfo>,
    tasks: Res<BackgroundTasks>,
    redactor: Res<SecretRedactor>,
    shell_approval: Res<ShellApproval>,
) {
    let StartedCall {
        activity,
        invocation,
        ..
    } = lifetime.take();
    let tool_call = &call.call;
    let name = &tool_call.function.name;
    let policy = &policy.permission;
    let redactor = *redactor;
    let turn = context.turn_marker();
    let receipt = call.receipt.clone();
    decision.record(&call, &store, turn);
    let block = {
        // A run the model deliberately detached is not holding the turn,
        // so it does not inherit the deadline that exists to stop a turn
        // hanging. The tool applies the same fallback to the request; this
        // keeps the table's view of the deadline equal to the real one.
        let deadline = if name == RUN_IN_BACKGROUND {
            tasks.background_deadline_for(&tool_call.function.arguments)
        } else {
            tasks.deadline_for(&tool_call.function.arguments)
        };
        let sink = invocation
            .sink(&session.session_id, session.turn_number, deadline)
            .with_call_root(CallRoot::new(&config.working_dir));
        // The turn's token reaches this command, and stops reaching it the
        // moment the user backgrounds it: a detached run is no longer part
        // of the turn, so Ctrl-C must not take it with the turn.
        let (detached_tx, detached_rx) = tokio::sync::watch::channel(false);
        let mut tool_context = ToolContext::new()
            .with(ShellSinkHandle::new(sink))
            .with(detachable_cancel(&cancel, detached_rx))
            .with(CallRoot::new(&config.working_dir));
        if let Some(receipt) = receipt {
            tool_context = tool_context.with(receipt);
        }
        let mut run = shell_approval.spawn(
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
            decision.push_result(&mut batch, tool_call.error(ToolResultContent::Text(output)));
            *lifetime = CallLifetime::Idle;
            batch.interrupted = true;
            finish_pending(
                &mut batch,
                &mut context,
                &user_io,
                &store,
                "not run: turn interrupted",
            )
            .await;
            return;
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
                    decision
                        .push_result(&mut batch, tool_call.error(ToolResultContent::Text(reason)));
                    return;
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
            decision.push_result(
                &mut batch,
                tool_call.result(ToolResultContent::Text(background_receipt(&task))),
            );
            return;
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
                if let Some(reason) = unattended_shell_refusal(name, &value, policy) {
                    activity.finish(&user_io, PHASE_DENIED, &reason).await;
                    record_tool_call(
                        &store,
                        turn,
                        name,
                        &tool_call.function.arguments,
                        "denied",
                        &reason,
                    );
                    decision
                        .push_result(&mut batch, tool_call.error(ToolResultContent::Text(reason)));
                    return;
                }
                // Every toolset in this crate answers with the string it
                // built; `run_command` answers with the executor's JSON
                // document. The document is what the trajectory keeps —
                // it is the honest record — but what
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
                // The terminal block shows a command the way a shell
                // would — real lines, collapsed behind its exit status
                // and size — rather than the escaped JSON document, which
                // rides along as metadata. The trajectory keeps `observed`.
                match (name == "run_command")
                    .then(|| run_command_outcome(&value))
                    .flatten()
                {
                    Some(outcome) => {
                        // A failed run keeps its output in view.
                        let outcome = run_command_succeeded(&value).then_some(outcome.as_str());
                        activity
                            .finish_rendered(&user_io, PHASE_OK, &output, outcome, &observed)
                            .await;
                    }
                    None => activity.finish(&user_io, PHASE_OK, &observed).await,
                }
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
                    batch.edited.push(config.working_dir.join(rel));
                }
                tool_call.result(ToolResultContent::Text(output))
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
                tool_call.error(ToolResultContent::Text(output))
            }
        }
    };
    decision.push_result(&mut batch, block);
}

#[system]
pub(crate) async fn advance_call(
    mut batch: ResMut<ToolBatch>,
    mut lifetime: ResMut<CallLifetime>,
) -> CallProgress {
    *lifetime = CallLifetime::Idle;
    batch.completed += 1;
    batch.progress()
}

async fn finish_pending(
    batch: &mut ToolBatch,
    context: &mut ContextManager,
    user_io: &UserIO,
    store: &OxigraphStore,
    reason: &str,
) {
    for (_, call) in batch.calls.drain(..) {
        let name = &call.function.name;
        let detail = tool_detail(name, &call.function.arguments);
        let activity = ToolActivity::start(user_io, &call.id, None, name.clone(), &detail).await;
        activity.finish(user_io, PHASE_ERROR, reason).await;
        record_tool_call(
            store,
            context.turn_marker(),
            name,
            &call.function.arguments,
            "error",
            reason,
        );
        batch
            .results
            .push(call.error(ToolResultContent::Text(reason.into())));
    }
    if !batch.results.is_empty() {
        context.push(Message::User {
            content: std::mem::take(&mut batch.results),
        });
    }
}

#[system]
pub(crate) async fn finish_calls(
    mut batch: ResMut<ToolBatch>,
    mut context: ResMut<ContextManager>,
    user_io: Res<UserIO>,
    store: Res<OxigraphStore>,
    config: Res<AgentConfig>,
) {
    // Also runs after the last permitted iteration. A batch cap must return an
    // error for every unrun call, never silently discard the queue.
    finish_pending(
        &mut batch,
        &mut context,
        &user_io,
        &store,
        "not run: batch limit reached",
    )
    .await;
    // Keep the Code `ABox` fresh: incrementally re-index every file an
    // `edit_file` mutated this turn, so spans don't drift after a write. Best
    // effort — a stale graph degrades navigation but must not fail the turn.
    if !batch.edited.is_empty() {
        match CodeIndexer::index_paths(&store, &config.working_dir, &batch.edited).await {
            Ok(n) => {
                tracing::debug!(
                    "code graph: re-indexed {} file(s), {n} facts",
                    batch.edited.len()
                );
                // Re-derive the conceptual-role layer so concept queries track
                // the edited source too. This rebuilds the whole enrichment graph
                // each edit; scoping it to re-indexed symbols is deferred.
                if let Err(error) = enrich(&store) {
                    tracing::warn!("code graph: enrichment after edit failed: {error}");
                }
            }
            Err(error) => tracing::warn!("code graph: incremental re-index failed: {error}"),
        }
    }

    batch.edited.clear();
}

pub(crate) fn build_tool_calls(graph: &mut Graph) {
    build_bounded_tool_calls(graph, TOOL_CALLS_BOUND);
}

fn build_bounded_tool_calls(graph: &mut Graph, bound: usize) {
    graph.add_system(prepare_calls);
    graph.add_system(dispatch_read_only_batch);
    graph.add_loop::<CallProgress, _, _>(
        "tool_calls",
        move |progress| progress.remaining == 0 || progress.completed >= bound,
        |g| {
            g.add_system(authorize_call);
            g.add_conditional_branch::<AuthorizedCall, _, _, _>(
                "needs_human",
                |call| matches!(call.route, AuthorizationRoute::Ask(_)),
                |ask| {
                    ask.add_system(ask_human);
                },
                |settle| {
                    settle.add_system(settle_authorization);
                },
            );
            g.add_conditional_branch::<CallDecision, _, _, _>(
                "call_allowed",
                |decision| matches!(decision, CallDecision::Allow(_)),
                |allow| {
                    allow.add_system(execute_call);
                },
                |deny| {
                    deny.add_system(record_denial);
                },
            );
            g.add_system(advance_call);
        },
    );
    if let Some(loop_id) = graph.last_node() {
        graph.set_loop_max_iterations(loop_id, bound);
    }
    graph.add_system(finish_calls);
}

#[cfg(test)]
mod tests;

mod cleanup;
pub(crate) use cleanup::{install_cleanup, setup_cleanup};
