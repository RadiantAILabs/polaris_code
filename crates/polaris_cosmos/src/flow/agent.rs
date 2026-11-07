//! Five-node isolated role call graph: reserve, stage, run, settle, collect.
use super::*;
use polaris::{
    graph::Graph,
    system::{
        param::{Out, Res},
        system,
    },
};
use serde_json::json;
use std::path::PathBuf;
#[derive(Clone)]
enum Reserved {
    Granted {
        call: AgentCall,
        role: Role,
        reservation: Reservation,
        workspace: PathBuf,
    },
    Done {
        call: AgentCall,
        outcome: AgentOutcome,
    },
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
        reservation: Reservation,
        failure: AgentFailure,
    },
    Done {
        call: AgentCall,
        outcome: AgentOutcome,
    },
}
#[derive(Clone)]
enum Called {
    Returned {
        call: AgentCall,
        workspace: PathBuf,
        reservation: Reservation,
        result: Result<AgentReport, AgentFailure>,
        limits: AgentLimits,
    },
    Done {
        call: AgentCall,
        outcome: AgentOutcome,
    },
}
#[derive(Clone)]
struct Settled {
    call: AgentCall,
    workspace: Option<PathBuf>,
    outcome: AgentOutcome,
}
fn internal(e: impl std::fmt::Display) -> AgentOutcome {
    AgentOutcome::Failed(AgentFailure::Internal(e.to_string()))
}
#[system]
async fn stage_node(
    value: Out<Reserved>,
    handle: Res<RunHandle>,
    settings: Res<FlowSettings>,
    phases: Res<PhaseControl>,
) -> Staged {
    match &*value {
        Reserved::Granted {
            call,
            role,
            reservation,
            workspace,
        } => match stage(
            call,
            role,
            workspace,
            reservation.granted_micro,
            &handle,
            &settings,
        ) {
            Ok(mut request) => {
                if let Some(remaining) = phases.remaining(&call.phase) {
                    request.limits.timeout = request.limits.timeout.min(remaining);
                }
                Staged::Ready {
                    call: call.clone(),
                    request,
                    reservation: reservation.clone(),
                }
            }
            Err(e) => Staged::Failed {
                call: call.clone(),
                reservation: reservation.clone(),
                failure: AgentFailure::Setup(e.to_string()),
            },
        },
        Reserved::Done { call, outcome } => Staged::Done {
            call: call.clone(),
            outcome: outcome.clone(),
        },
    }
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
            let result = runner.0.run(request.clone()).await;
            Called::Returned {
                call: call.clone(),
                workspace: request.workspace.clone(),
                limits: request.limits.clone(),
                reservation: reservation.clone(),
                result,
            }
        }
        Staged::Failed {
            call,
            reservation,
            failure,
        } => Called::Returned {
            call: call.clone(),
            workspace: PathBuf::new(),
            reservation: reservation.clone(),
            result: Err(failure.clone()),
            limits: AgentLimits {
                max_turns: 0,
                max_micros: reservation.granted_micro,
                timeout: std::time::Duration::ZERO,
                max_output_tokens: None,
            },
        },
        Staged::Done { call, outcome } => Called::Done {
            call: call.clone(),
            outcome: outcome.clone(),
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
    match &*value {
        Called::Done { call, outcome } => Settled {
            call: call.clone(),
            workspace: None,
            outcome: outcome.clone(),
        },
        Called::Returned {
            call,
            workspace,
            reservation,
            result,
            limits,
        } => {
            let (usage, status, outcome) = match result {
                Ok(report) => {
                    let mut report = report.clone();
                    if report.usage.model_id.is_empty() {
                        report.usage.model_id = settings.model_id.clone();
                    }
                    if report.stop == StopCause::Interrupted {
                        let _ = phases.interrupt(&call.phase);
                    }
                    let status = status(&report.stop);
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
                    StopCause::TurnCap => {
                        Some(("turns", limits.max_turns as u64, usage.requests as u64))
                    }
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
                Err(e) => internal(e),
            };
            let _=events.emit("agent_end",Some(&call.phase),json!({"step":call.step,"role":call.role,"workspace":workspace,"stop":status,"turns":usage.requests,"usd":dollars(usage.micros)}));
            Settled {
                call: call.clone(),
                workspace: Some(workspace.clone()),
                outcome,
            }
        }
    }
}
#[system]
async fn collect_node(
    value: Out<Settled>,
    journal: Res<StepJournal>,
    control: Option<Res<PhaseControl>>,
) -> AgentOutcome {
    let mut outcome = value.outcome.clone();
    if let (Some(workspace), AgentOutcome::Ran { outputs, .. }) = (&value.workspace, &mut outcome) {
        for expected in &value.call.expect {
            let rel = match relative(expected) {
                Ok(rel) => rel,
                Err(e) => return internal(e),
            };
            let path = workspace.join(rel);
            if path.exists()
                && !path
                    .canonicalize()
                    .map_err(|e| e.to_string())
                    .is_ok_and(|p| {
                        workspace
                            .canonicalize()
                            .is_ok_and(|root| p.starts_with(root))
                    })
            {
                return internal("output outside call workspace");
            }
            if !path.exists() {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => {
                    match std::fs::read(&path) {
                        Ok(bytes) => outputs.push(OutputFile {
                            path: expected.clone(),
                            sha256: digest(&bytes),
                            bytes: bytes.len() as u64,
                        }),
                        Err(e) => return internal(e),
                    }
                }
                Ok(_) => return internal("expected output must be an ordinary file"),
                Err(e) => return internal(e),
            }
        }
    }
    let interrupted = matches!(&outcome,AgentOutcome::Ran{report,..}if report.stop==StopCause::Interrupted)
        || control.is_some_and(|control| control.halt_reason().as_deref() == Some("interrupted"));
    if !interrupted {
        if let Err(e) = journal.put(
            &value.call.step,
            serde_json::to_value(&outcome).expect("serializable outcome"),
        ) {
            return internal(e);
        }
    } else {
        let _ = journal.edge(&value.call.step, "failed");
    }
    outcome
}
/// Builds a five-node role-call subgraph. The preceding system must emit `AgentCall`.
/// The role key is fixed in the graph and can be inspected using `agent_roles`.
/// Expected call failures are values, so parallel siblings always finish and settle.
pub fn agent(id: &str, role: &str) -> Graph {
    let mut graph = Graph::new();
    let expected_role = role.to_owned();
    graph
        .system(super::bind::<
            (
                Out<AgentCall>,
                Res<RoleBook>,
                Res<SpendBook>,
                Res<StepJournal>,
                Res<RunHandle>,
                Res<PhaseControl>,
                Res<EventLog>,
                Option<Res<CallDeadline>>,
            ),
            _,
            _,
        >(
            "agent.reserve",
            move |(call, roles, spend, journal, handle, phases, events, deadline), _| {
                let expected_role = expected_role.clone();
                Box::pin(async move {
                    let call = (*call).clone();
                    let result = (|| -> FlowResult<Reserved> {
                        if call.role != expected_role {
                            return Err(FlowError(format!(
                                "role {} does not match graph role {expected_role}",
                                call.role
                            )));
                        }
                        if handle.mode == "resume"
                            && let Some(record) =
                                journal.read(&call.step)?.filter(|r| r.status == "done")
                        {
                            return Ok(Reserved::Done {
                                call: call.clone(),
                                outcome: serde_json::from_value(record.output)?,
                            });
                        }
                        let role = roles
                            .roles
                            .get(&call.role)
                            .ok_or_else(|| FlowError(format!("missing role {}", call.role)))?
                            .clone();
                        if let Some(deadline)=deadline && deadline.expired() {
                            let _ = events.emit(
                                "cap_hit",
                                Some(&call.phase),
                                json!({"cap":"wall_seconds","limit":deadline.limit_seconds(),"used":deadline.elapsed_seconds(),"step":call.step}),
                            );
                            let _=events.emit("step_skipped",Some(&call.phase),json!({"step":call.step,"reason":"budget"}));
                            return Ok(Reserved::Done {
                                call: call.clone(),
                                outcome: AgentOutcome::Skipped(SkipReason::Budget),
                            });
                        }
                        if phases.expired(&call.phase) {
                            phases.set_status(
                                &call.phase,
                                PhaseStatus::Failed("phase deadline".into()),
                            )?;
                        }
                        if phases.halted() || phases.expired(&call.phase) {
                            return Ok(Reserved::Done {
                                call: call.clone(),
                                outcome: AgentOutcome::Skipped(SkipReason::Disabled),
                            });
                        }
                        let workspace = journal.workspace(&call.step)?;
                        let artifact = workspace
                            .strip_prefix(&handle.root)
                            .map_err(|_| FlowError("workspace outside run".into()))?
                            .display()
                            .to_string();
                        let reservation = match spend.reserve_call(
                            &call.phase,
                            &call.role,
                            role.max_micros()?,
                            &call.step,
                            &artifact,
                        ) {
                            Ok(res) => res,
                            Err(_) => {
                                let _=events.emit("cap_hit",Some(&call.phase),json!({"cap":format!("budget.{}",call.phase),"limit":spend.phase_cap(&call.phase),"used":spend.committed(),"step":call.step}));
                                let _ = events.emit(
                                    "step_skipped",
                                    Some(&call.phase),
                                    json!({"step":call.step,"reason":"budget"}),
                                );
                                return Ok(Reserved::Done {
                                    call: call.clone(),
                                    outcome: AgentOutcome::Skipped(SkipReason::Budget),
                                });
                            }
                        };
                        journal.edge(&call.step, "started")?;
                        Ok(Reserved::Granted {
                            call: call.clone(),
                            role,
                            reservation,
                            workspace,
                        })
                    })();
                    Ok(result.unwrap_or_else(|e| Reserved::Done {
                        call,
                        outcome: internal(e),
                    }))
                })
            },
        ))
        .named(format!("agent.reserve[{role}]/{id}"))
        .done();
    graph.system(stage_node).named(format!("{id}.stage")).done();
    graph.system(run_node).named(format!("{id}.run")).done();
    graph
        .system(settle_node)
        .named(format!("{id}.settle"))
        .done();
    graph
        .system(collect_node)
        .named(format!("{id}.collect"))
        .done();
    graph
}
/// Lists every graph-bound role, including roles inside nested scopes.
pub fn agent_roles(graph: &Graph) -> std::collections::BTreeSet<String> {
    let mut roles = std::collections::BTreeSet::new();
    for node in graph.nodes() {
        let name = node.name().to_string();
        if let Some(rest) = name.strip_prefix("agent.reserve[")
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
