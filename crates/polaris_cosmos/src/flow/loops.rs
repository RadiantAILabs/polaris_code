//! Native graph loops composed over persistent counters and queues.
use super::*;
use polaris::{
    graph::{CaughtError, ContextPolicy, Graph},
    system::{
        param::{ErrOut, Out, Res},
        system::SystemError,
    },
};
#[derive(Clone)]
struct RepairGate {
    done: bool,
}
/// Builds a check/repair loop with a named counter shared by every repair edge.
/// The check graph emits `Verdict`; each failed check consumes one repair unit.
/// An exhausted counter exits with the last failing verdict.
///
/// The counter must be declared in [`RepairCounters`]; an undeclared name fails
/// the loop's decision system.
///
/// # Panics
///
/// Panics when `check` or `repair` is not a well-formed graph: every node must
/// be reachable from its entry. An empty graph is accepted.
pub fn repair_loop(name: &str, counter: &str, check: Graph, repair: Graph) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(|| async { RepairGate { done: false } });
    let counter = counter.to_owned();
    graph.add_loop::<RepairGate, _, _>(
        name.to_owned(),
        |s| s.done,
        move |g| {
            g.append(check).expect("check must be a connected graph");
            let gate_counter = counter.clone();
            g.add_system(bind::<(Out<Verdict>, Res<RepairCounters>,Option<Res<EventLog>>), _, _>(
                "repair.decide",
                move |(verdict, counters,events), _| {
                    let counter = gate_counter.clone();
                    Box::pin(async move {
                        let exhausted = !counters.has_room(&counter)?;
                        if matches!(&*verdict,Verdict::Fail(_))&&exhausted&&let Some(events)=events{events.emit("cap_hit",None,serde_json::json!({"cap":counter,"limit":counters.limit(&counter),"used":counters.used(&counter),"step":counter}))?;}
                        Ok(RepairGate {
                            done: matches!(&*verdict, Verdict::Pass) || exhausted,
                        })
                    })
                },
            ));
            g.add_conditional_branch::<RepairGate, _, _, _>(
                "repair.needed",
                |s| !s.done,
                move |r| {
                    r.add_system(bind::<Res<RepairCounters>, (), _>(
                        "repair.consume",
                        move |counters, _| {
                            let counter = counter.clone();
                            Box::pin(async move {
                                counters.consume(&counter)?;
                                Ok(())
                            })
                        },
                    ));
                    r.append(repair).expect("repair must be a connected graph");
                },
                |r| {
                    r.add_system(|| async {});
                },
            );
        },
    );
    graph
}
/// Builds a fixed-width parallel fork. Each lane drains the same lease book.
/// A failed lane body takes its own error edge, acknowledges its lease and ends
/// that lane with a failing `Verdict` that carries the error; parallel siblings
/// continue until the shared book is drained.
/// An interrupted lane ends with its lease still pending for a resumed run.
pub fn lease_loop<F>(name: &str, width: usize, build: F) -> Graph
where
    F: Fn(usize) -> Graph,
{
    let branches = (0..width)
        .map(|worker| {
            let mut lane = Graph::new();
            lane.add_system(|| async {
                LaneLease {
                    worker: 0,
                    lease: Some(Lease {
                        global: 0,
                        local: 0,
                    }),
                }
            });
            let mut body = build(worker);
            body.add_error_handler(|g| {
                g.add_system(bind::<ErrOut<CaughtError>, Verdict, _>(
                    "lane.failure",
                    |error, _| Box::pin(async move { Ok(Verdict::Fail(error.to_string())) }),
                ));
            });
            lane.add_loop::<LaneLease, _, _>(
                format!("lane/{worker}"),
                |s| s.lease.is_none(),
                move |g| {
                    g.add_system(bind::<(Res<LeaseBook>, Option<Res<PhaseControl>>), _, _>(
                        "lease.take",
                        move |(book, control), _| {
                            Box::pin(async move {
                                Ok(LaneLease {
                                    worker,
                                    lease: if control.is_some_and(|control| control.halted()) {
                                        None
                                    } else {
                                        book.take(worker)?
                                    },
                                })
                            })
                        },
                    ));
                    g.add_conditional_branch::<LaneLease, _, _, _>(
                        "lease.granted",
                        |s| s.lease.is_some(),
                        move |g| {
                            g.add_system(|| async { Verdict::Pass });
                            g.add_scope("lane.body", body, ContextPolicy::shared());
                            g.add_system(bind::<
                                (
                                    Res<LeaseBook>,
                                    Out<LaneLease>,
                                    Out<Verdict>,
                                    Option<Res<PhaseControl>>,
                                ),
                                LaneLease,
                                _,
                            >(
                                "lease.finish",
                                move |(book, current, verdict, control), _| {
                                    Box::pin(async move {
                                        if control.is_some_and(|control| control.interrupted()) {
                                            return Ok(LaneLease {
                                                worker,
                                                lease: None,
                                            });
                                        }
                                        book.finish(worker)?;
                                        Ok(LaneLease {
                                            worker,
                                            lease: if matches!(&*verdict, Verdict::Fail(_)) {
                                                None
                                            } else {
                                                current.lease.clone()
                                            },
                                        })
                                    })
                                },
                            ));
                        },
                        |g| {
                            g.add_system(|| async {});
                        },
                    );
                },
            );
            Box::new(move |g: &mut Graph| {
                g.append(lane).expect("a lane built here is connected");
            }) as Box<dyn FnOnce(&mut Graph)>
        })
        .collect::<Vec<_>>();
    let mut graph = Graph::new();
    graph.add_parallel(name.to_owned(), branches);
    graph
}
/// Current item supplied to the body of `worklist`.
#[derive(Clone, Debug)]
pub struct CurrentItem(Option<WorkItem>);
impl CurrentItem {
    /// The item being worked on, or none once the queue is drained.
    pub fn item(&self) -> Option<&WorkItem> {
        self.0.as_ref()
    }
}
/// Builds a sequential queue drain. The body emits `Verdict`; errors are recorded
/// as failed item values and the next persisted item is attempted.
/// Interruption keeps the active item pending and prevents further item takes.
pub fn worklist(name: &str, mut body: Graph) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(|| async {
        CurrentItem(Some(WorkItem {
            key: StepKey::new("seed").expect("`seed` is a valid step key"),
            value: serde_json::Value::Null,
            status: WorkItemStatus::Pending,
            note: String::new(),
        }))
    });
    body.add_error_handler(|g| {
        g.add_system(bind::<ErrOut<CaughtError>, Verdict, _>(
            "item.failure",
            |error, _| Box::pin(async move { Ok(Verdict::Fail(error.to_string())) }),
        ));
    });
    graph.add_loop::<CurrentItem, _, _>(
        name.to_owned(),
        |item| item.item().is_none(),
        move |g| {
            g.add_system(bind::<(Res<Worklist>, Option<Res<PhaseControl>>), _, _>(
                "item.take",
                |(list, control), _| {
                    Box::pin(async move {
                        Ok(CurrentItem(
                            if control.is_some_and(|control| control.interrupted()) {
                                None
                            } else {
                                list.take()
                            },
                        ))
                    })
                },
            ));
            g.add_conditional_branch::<CurrentItem, _, _, _>(
                "item.available",
                |item| item.item().is_some(),
                move |g| {
                    g.add_scope("item.body", body, ContextPolicy::shared());
                    g.add_system(bind::<
                        (
                            Out<CurrentItem>,
                            Out<Verdict>,
                            Res<Worklist>,
                            Option<Res<PhaseControl>>,
                        ),
                        (),
                        _,
                    >(
                        "item.finish",
                        |(item, verdict, list, control), _| {
                            Box::pin(async move {
                                if control.is_some_and(|control| control.interrupted()) {
                                    return Ok(());
                                }
                                let current = item.item().ok_or_else(|| {
                                    SystemError::ExecutionError(
                                        "item.finish ran without a current item".into(),
                                    )
                                })?;
                                list.finish(
                                    &current.key,
                                    match &*verdict {
                                        Verdict::Pass => Ok(()),
                                        Verdict::Fail(note) => Err(note.clone()),
                                    },
                                )?;
                                Ok(())
                            })
                        },
                    ));
                },
                |g| {
                    g.add_system(|| async {});
                },
            );
        },
    );
    graph
}
/// Builds bounded rounds with an explicit join barrier before each per-round graph.
///
/// # Panics
///
/// Panics when `body` or `barrier` is not a well-formed graph: every node must
/// be reachable from its entry. An empty graph is accepted.
pub fn rounds(name: &str, count: usize, body: Graph, barrier: Graph) -> Graph {
    let mut graph = Graph::new();
    graph.add_loop_n(name.to_owned(), count, move |g| {
        g.append(body).expect("body must be a connected graph");
        g.append(barrier)
            .expect("barrier must be a connected graph");
    });
    graph
}
