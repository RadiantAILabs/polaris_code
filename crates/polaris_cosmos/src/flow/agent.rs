//! Isolated role call graph: replay, admit, reserve, then stage, run, settle and collect.
use super::io::{hash_file, relative};
use super::*;
use polaris::{
    graph::Graph,
    system::{
        param::{Out, Res},
        system,
        system::SystemError,
    },
};
use serde_json::json;
use std::{
    io,
    path::{Path, PathBuf},
};
/// What the resume probe found for a call: an outcome restored from the
/// journal, or nothing.
#[derive(Clone)]
struct Replayed {
    call: AgentCall,
    restored: Option<AgentOutcome>,
}
/// A call that ends before the runner is reached, with the journal state to record.
#[derive(Clone)]
struct Ended {
    // Boxed so that `Err(Ended)` stays small.
    outcome: Box<AgentOutcome>,
    status: StepStatus,
}
impl Ended {
    fn failed(reason: impl std::fmt::Display) -> Self {
        Self {
            outcome: Box::new(internal(reason)),
            status: StepStatus::Failed,
        }
    }
    fn skipped(reason: SkipReason) -> Self {
        Self {
            outcome: Box::new(AgentOutcome::Skipped(reason)),
            status: StepStatus::Skipped,
        }
    }
}
/// Whether a call may start: `ended` is set when it may not.
#[derive(Clone)]
struct Admission {
    call: AgentCall,
    ended: Option<Ended>,
}
#[derive(Clone)]
struct Granted {
    call: AgentCall,
    role: Role,
    reservation: Reservation,
    workspace: PathBuf,
}
#[derive(Clone)]
enum Reserved {
    Granted(Box<Granted>),
    Declined { call: AgentCall, ended: Ended },
}
#[derive(Clone)]
enum Staged {
    Ready {
        call: AgentCall,
        request: AgentRequest,
        reservation: Reservation,
    },
    Failed {
        call: AgentCall,
        workspace: PathBuf,
        reservation: Reservation,
        failure: AgentFailure,
    },
}
#[derive(Clone)]
struct Called {
    call: AgentCall,
    workspace: PathBuf,
    reservation: Reservation,
    result: Result<AgentReport, AgentFailure>,
    limits: AgentLimits,
}
#[derive(Clone)]
struct Settled {
    call: AgentCall,
    workspace: PathBuf,
    outcome: AgentOutcome,
}
fn internal(reason: impl std::fmt::Display) -> AgentOutcome {
    AgentOutcome::Failed(AgentFailure::Internal(reason.to_string()))
}
/// Records how a call that never reached the runner ended and returns its outcome.
fn end(journal: &StepJournal, call: &AgentCall, ended: &Ended) -> AgentOutcome {
    match journal.edge(&call.step, ended.status) {
        Ok(()) => (*ended.outcome).clone(),
        Err(error) => internal(error),
    }
}
/// Restores a completed call from the journal, if the call has one.
fn restore(journal: &StepJournal, call: &AgentCall) -> FlowResult<Option<AgentOutcome>> {
    match journal
        .read(&call.step)?
        .filter(|record| record.status == StepStatus::Done)
    {
        Some(record) => Ok(Some(serde_json::from_value(record.output)?)),
        None => Ok(None),
    }
}
/// Decides whether a call may start. Role, wall-clock and phase checks come
/// first; the budget is decided when the reservation is attempted.
fn admit(
    call: &AgentCall,
    expected_role: &str,
    phases: &PhaseControl,
    events: &EventLog,
    deadline: Option<&CallDeadline>,
) -> FlowResult<Option<Ended>> {
    if call.role != expected_role {
        return Ok(Some(Ended::failed(format!(
            "role {} does not match graph role {expected_role}",
            call.role
        ))));
    }
    if let Some(deadline) = deadline
        && deadline.expired()
    {
        let _ = events.emit(
            "cap_hit",
            Some(&call.phase),
            json!({"cap":"wall_seconds","limit":deadline.limit_seconds(),"used":deadline.elapsed_seconds(),"step":call.step}),
        );
        let _ = events.emit(
            "step_skipped",
            Some(&call.phase),
            json!({"step":call.step,"reason":"budget"}),
        );
        return Ok(Some(Ended::skipped(SkipReason::Budget)));
    }
    if phases.expired(&call.phase) {
        phases.set_status(&call.phase, PhaseStatus::Failed("phase deadline".into()))?;
    }
    if phases.halted() || phases.expired(&call.phase) {
        return Ok(Some(Ended::skipped(SkipReason::Disabled)));
    }
    Ok(None)
}
/// Reserves the role's ceiling and journals the start, or says why the call ends.
///
/// Only a refused reservation is a budget skip. A fault while saving the
/// reservation or the journal edge is a failure. When the start cannot be
/// journaled the reservation is released, so a call that never ran holds no
/// allowance.
fn reserve(
    call: &AgentCall,
    roles: &RoleBook,
    spend: &SpendBook,
    journal: &StepJournal,
    handle: &RunHandle,
    events: &EventLog,
) -> Result<Granted, Ended> {
    let role = roles
        .roles
        .get(&call.role)
        .ok_or_else(|| Ended::failed(format!("missing role {}", call.role)))?
        .clone();
    let workspace = journal.workspace(&call.step).map_err(Ended::failed)?;
    let artifact = workspace
        .strip_prefix(&handle.root)
        .map_err(|_| Ended::failed("workspace outside run"))?
        .display()
        .to_string();
    let reservation = match spend.reserve_call(
        &call.phase,
        &call.role,
        role.max_micros(),
        &call.step,
        &artifact,
    ) {
        Ok(reservation) => reservation,
        Err(ReserveError::Refused(_)) => {
            let _ = events.emit(
                "cap_hit",
                Some(&call.phase),
                json!({"cap":format!("budget.{}",call.phase),"limit":spend.phase_cap(&call.phase),"used":spend.committed(),"step":call.step}),
            );
            let _ = events.emit(
                "step_skipped",
                Some(&call.phase),
                json!({"step":call.step,"reason":"budget"}),
            );
            return Err(Ended::skipped(SkipReason::Budget));
        }
        Err(fault) => return Err(Ended::failed(fault)),
    };
    if let Err(error) = journal.edge(&call.step, StepStatus::Started) {
        // If the release fails too, recovery at the next start charges the
        // reservation at its ceiling.
        let _ = spend.release(reservation);
        return Err(Ended::failed(error));
    }
    Ok(Granted {
        call: call.clone(),
        role,
        reservation,
        workspace,
    })
}
#[system]
async fn restore_node(value: Out<Replayed>) -> AgentOutcome {
    value
        .restored
        .clone()
        .unwrap_or_else(|| internal("replay branch taken without a restored outcome"))
}
#[system]
async fn skip_node(value: Out<Admission>, journal: Res<StepJournal>) -> AgentOutcome {
    match &value.ended {
        Some(ended) => end(&journal, &value.call, ended),
        None => internal("skip branch taken for an admitted call"),
    }
}
#[system]
async fn decline_node(value: Out<Reserved>, journal: Res<StepJournal>) -> AgentOutcome {
    match &*value {
        Reserved::Declined { call, ended } => end(&journal, call, ended),
        Reserved::Granted(granted) => end(
            &journal,
            &granted.call,
            &Ended::failed("decline branch taken with a granted reservation"),
        ),
    }
}
#[system]
async fn stage_node(
    value: Out<Reserved>,
    handle: Res<RunHandle>,
    settings: Res<FlowSettings>,
    phases: Res<PhaseControl>,
) -> Result<Staged, SystemError> {
    let Reserved::Granted(granted) = &*value else {
        return Err(SystemError::ExecutionError(
            "stage ran without a granted reservation".into(),
        ));
    };
    let call = granted.call.clone();
    let role = granted.role.clone();
    let workspace = granted.workspace.clone();
    let ceiling = granted.reservation.granted_micro;
    let (handle, settings) = ((*handle).clone(), (*settings).clone());
    // Staging copies trees and writes files, so it runs off the async threads.
    let staging = tokio::task::spawn_blocking(move || {
        stage(&call, &role, &workspace, ceiling, &handle, &settings)
    })
    .await;
    let call = granted.call.clone();
    let reservation = granted.reservation.clone();
    let failure = match staging {
        Ok(Ok(mut request)) => {
            if let Some(remaining) = phases.remaining(&call.phase) {
                request.limits.timeout = request.limits.timeout.min(remaining);
            }
            return Ok(Staged::Ready {
                call,
                request,
                reservation,
            });
        }
        Ok(Err(error)) => AgentFailure::Setup(error.to_string()),
        Err(error) => AgentFailure::Setup(format!("staging task failed: {error}")),
    };
    Ok(Staged::Failed {
        call,
        workspace: granted.workspace.clone(),
        reservation,
        failure,
    })
}
#[system]
async fn run_node(value: Out<Staged>, runner: Res<AgentRunner>, events: Res<EventLog>) -> Called {
    match &*value {
        Staged::Ready {
            call,
            request,
            reservation,
        } => {
            let _ = events.emit(
                "agent_start",
                Some(&call.phase),
                json!({"step":call.step,"role":call.role,"workspace":request.workspace}),
            );
            let result = runner.runner().run(request.clone()).await;
            Called {
                call: call.clone(),
                workspace: request.workspace.clone(),
                limits: request.limits.clone(),
                reservation: reservation.clone(),
                result,
            }
        }
        Staged::Failed {
            call,
            workspace,
            reservation,
            failure,
        } => Called {
            call: call.clone(),
            workspace: workspace.clone(),
            reservation: reservation.clone(),
            result: Err(failure.clone()),
            limits: AgentLimits {
                max_turns: 0,
                max_micros: reservation.granted_micro,
                timeout: std::time::Duration::ZERO,
                max_output_tokens: None,
            },
        },
    }
}
#[system]
async fn settle_node(
    value: Out<Called>,
    spend: Res<SpendBook>,
    phases: Res<PhaseControl>,
    settings: Res<FlowSettings>,
    events: Res<EventLog>,
) -> Settled {
    let Called {
        call,
        workspace,
        reservation,
        result,
        limits,
    } = &*value;
    let (usage, status, outcome) = match result {
        Ok(report) => {
            let mut report = report.clone();
            if report.usage.model_id.is_empty() {
                report.usage.model_id = settings.model_id.clone();
            }
            if report.stop == StopCause::Interrupted {
                let _ = phases.interrupt(&call.phase);
            }
            let status = report.stop.ledger_status();
            (
                report.usage.clone(),
                status,
                AgentOutcome::Ran {
                    report,
                    outputs: Vec::new(),
                },
            )
        }
        Err(failure) => {
            let micros = if matches!(failure, AgentFailure::Setup(_)) {
                0
            } else {
                reservation.granted_micro
            };
            (
                Usage {
                    model_id: settings.model_id.clone(),
                    micros,
                    ..Usage::default()
                },
                "failed",
                AgentOutcome::Failed(failure.clone()),
            )
        }
    };
    if let Ok(report) = result {
        let cap = match report.stop {
            StopCause::TurnCap => Some(("turns", limits.max_turns as u64, usage.requests as u64)),
            StopCause::CostCap => Some(("micro_usd", limits.max_micros, usage.micros)),
            StopCause::Timeout => Some((
                "timeout_ms",
                limits.timeout.as_millis() as u64,
                limits.timeout.as_millis() as u64,
            )),
            _ => None,
        };
        if let Some((cap, limit, used)) = cap {
            let _=events.emit("cap_hit",Some(&call.phase),json!({"cap":format!("{}.{cap}",call.role),"limit":limit,"used":used,"step":call.step}));
        }
    }
    let outcome = match spend.settle_status(reservation, &usage, status) {
        Ok(()) => outcome,
        Err(error) => internal(error),
    };
    let _=events.emit("agent_end",Some(&call.phase),json!({"step":call.step,"role":call.role,"workspace":workspace,"stop":status,"turns":usage.requests,"usd":dollars(usage.micros)}));
    Settled {
        call: call.clone(),
        workspace: workspace.clone(),
        outcome,
    }
}
/// Hashes every expected output that exists, refusing anything that is not an
/// ordinary file inside the workspace or that is larger than `cap` bytes.
fn collect_outputs(workspace: &Path, expect: &[String], cap: u64) -> FlowResult<Vec<OutputFile>> {
    let root = workspace.canonicalize()?;
    let mut outputs = Vec::new();
    for expected in expect {
        let path = workspace.join(relative(expected)?);
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if !meta.is_file() {
            return Err(FlowError::invalid(
                "expected output must be an ordinary file",
            ));
        }
        if !path.canonicalize()?.starts_with(&root) {
            return Err(FlowError::invalid("output outside call workspace"));
        }
        let (sha256, bytes) = hash_file(&path, cap)?;
        outputs.push(OutputFile {
            path: expected.clone(),
            sha256,
            bytes,
        });
    }
    Ok(outputs)
}
#[system]
async fn collect_node(
    value: Out<Settled>,
    journal: Res<StepJournal>,
    control: Option<Res<PhaseControl>>,
    settings: Res<FlowSettings>,
) -> AgentOutcome {
    let mut outcome = value.outcome.clone();
    if matches!(outcome, AgentOutcome::Ran { .. }) {
        let workspace = value.workspace.clone();
        let expect = value.call.expect.clone();
        let cap = settings.max_output_bytes;
        // Hashing reads whole files, so it runs off the async threads.
        let collected =
            tokio::task::spawn_blocking(move || collect_outputs(&workspace, &expect, cap)).await;
        match collected {
            Ok(Ok(files)) => {
                if let AgentOutcome::Ran { outputs, .. } = &mut outcome {
                    outputs.extend(files);
                }
            }
            Ok(Err(error)) => outcome = internal(error),
            Err(error) => outcome = internal(format!("output collection task failed: {error}")),
        }
    }
    let interrupted = matches!(&outcome,AgentOutcome::Ran{report,..}if report.stop==StopCause::Interrupted)
        || control.is_some_and(|control| control.interrupted());
    // Only a completed call is journaled as done. A failed or interrupted call
    // stays not-done, so a resumed run runs it again.
    if matches!(outcome, AgentOutcome::Ran { .. }) && !interrupted {
        let saved = serde_json::to_value(&outcome)
            .map_err(FlowError::from)
            .and_then(|stored| journal.put(&value.call.step, stored));
        if let Err(error) = saved {
            return internal(error);
        }
    } else {
        let _ = journal.edge(&value.call.step, StepStatus::Failed);
    }
    outcome
}
/// Builds a role-call subgraph. The preceding system must emit `AgentCall`.
/// The role key is fixed in the graph and can be inspected using `agent_roles`.
/// Expected call failures are values, so parallel siblings always finish and settle.
///
/// The subgraph chooses one of several paths with graph branches, and every
/// path ends by emitting an `AgentOutcome`:
///
/// - A resumed run finds the call done in the journal and restores its outcome.
/// - A call that fails the role check, is past a wall-clock deadline, or belongs
///   to a halted or expired phase ends as `Skipped` or `Failed` without
///   reserving anything.
/// - A call whose budget is refused ends as `Skipped(Budget)`; a fault while
///   reserving ends as `Failed`.
/// - Otherwise the call is reserved, staged, run, settled and collected.
///
/// Only a call that ran to a stop is journaled as done, and only a resumed run
/// ([`RunMode::Resume`]) restores it. Skipped, failed and interrupted calls are
/// journaled as not done, so a resumed run tries them again.
///
/// The step key names the call in the journal. A call that is already done is
/// restored for every later pass in a resumed run, so reusing one key for
/// several passes, such as the iterations of a loop, restores the first result
/// instead of running again. Use a distinct key for each pass that must run.
/// Without a resume each pass runs and gets a fresh numbered workspace.
pub fn agent(id: &str, role: &str) -> Graph {
    let mut graph = Graph::new();
    graph
        .system(super::bind::<
            (Out<AgentCall>, Res<RunHandle>, Res<StepJournal>),
            _,
            _,
        >("agent.replay", |(call, handle, journal), _| {
            Box::pin(async move {
                let call = (*call).clone();
                let restored = if handle.mode == RunMode::Resume {
                    restore(&journal, &call).unwrap_or_else(|error| Some(internal(error)))
                } else {
                    None
                };
                Ok(Replayed { call, restored })
            })
        }))
        .named(format!("agent.replay[{role}]/{id}"))
        .done();
    let restore_id = id.to_owned();
    let admission_id = id.to_owned();
    let expected_role = role.to_owned();
    graph.add_conditional_branch::<Replayed, _, _, _>(
        format!("{id}.replayed"),
        |replayed| replayed.restored.is_some(),
        move |g| {
            g.system(restore_node)
                .named(format!("{restore_id}.restore"))
                .done();
        },
        move |g| admission_path(g, &admission_id, expected_role),
    );
    graph
}
/// Wires the admission check and the paths it chooses between.
fn admission_path(graph: &mut Graph, id: &str, expected_role: String) {
    graph
        .system(super::bind::<
            (
                Out<Replayed>,
                Res<PhaseControl>,
                Res<EventLog>,
                Option<Res<CallDeadline>>,
            ),
            _,
            _,
        >(
            "agent.admit",
            move |(replayed, phases, events, deadline), _| {
                let expected_role = expected_role.clone();
                Box::pin(async move {
                    let call = replayed.call.clone();
                    let ended = admit(&call, &expected_role, &phases, &events, deadline.as_deref())
                        .unwrap_or_else(|error| Some(Ended::failed(error)));
                    Ok(Admission { call, ended })
                })
            },
        ))
        .named(format!("{id}.admit"))
        .done();
    let skip_id = id.to_owned();
    let reserve_id = id.to_owned();
    graph.add_conditional_branch::<Admission, _, _, _>(
        format!("{id}.admitted"),
        |admission| admission.ended.is_none(),
        move |g| reservation_path(g, &reserve_id),
        move |g| {
            g.system(skip_node).named(format!("{skip_id}.skip")).done();
        },
    );
}
/// Wires the reservation and the paths it chooses between.
fn reservation_path(graph: &mut Graph, id: &str) {
    graph
        .system(super::bind::<
            (
                Out<Admission>,
                Res<RoleBook>,
                Res<SpendBook>,
                Res<StepJournal>,
                Res<RunHandle>,
                Res<EventLog>,
            ),
            _,
            _,
        >(
            "agent.reserve",
            |(admission, roles, spend, journal, handle, events), _| {
                Box::pin(async move {
                    let call = &admission.call;
                    Ok(
                        match reserve(call, &roles, &spend, &journal, &handle, &events) {
                            Ok(granted) => Reserved::Granted(Box::new(granted)),
                            Err(ended) => Reserved::Declined {
                                call: call.clone(),
                                ended,
                            },
                        },
                    )
                })
            },
        ))
        .named(format!("{id}.reserve"))
        .done();
    let run_id = id.to_owned();
    let decline_id = id.to_owned();
    graph.add_conditional_branch::<Reserved, _, _, _>(
        format!("{id}.granted"),
        |reserved| matches!(reserved, Reserved::Granted(_)),
        move |g| {
            g.system(stage_node).named(format!("{run_id}.stage")).done();
            g.system(run_node).named(format!("{run_id}.run")).done();
            g.system(settle_node)
                .named(format!("{run_id}.settle"))
                .done();
            g.system(collect_node)
                .named(format!("{run_id}.collect"))
                .done();
        },
        move |g| {
            g.system(decline_node)
                .named(format!("{decline_id}.decline"))
                .done();
        },
    );
}
/// Lists every graph-bound role, including roles inside nested scopes.
pub fn agent_roles(graph: &Graph) -> std::collections::BTreeSet<String> {
    let mut roles = std::collections::BTreeSet::new();
    for node in graph.nodes() {
        let name = node.name().to_string();
        if let Some(rest) = name.strip_prefix("agent.replay[")
            && let Some((role, _)) = rest.split_once(']')
        {
            roles.insert(role.into());
        }
        if let polaris::graph::node::Node::Scope(scope) = node {
            roles.extend(agent_roles(scope.graph()));
        }
    }
    roles
}
