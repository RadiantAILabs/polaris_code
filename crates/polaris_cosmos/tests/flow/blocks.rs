use super::*;
use polaris::graph::hooks::{HooksAPI, events::GraphEvent, schedule::OnSystemStart};
use std::sync::Mutex;

#[derive(Clone, Default)]
struct ExecutionLog(Arc<Mutex<Vec<String>>>);
impl ExecutionLog {
    fn hooks(&self) -> HooksAPI {
        let hooks = HooksAPI::new();
        let log = self.0.clone();
        hooks
            .register_observer::<OnSystemStart, _>("execution_log", move |event| {
                if let GraphEvent::SystemStart { node_name, .. } = event {
                    log.lock().unwrap().push(node_name.to_string());
                }
            })
            .unwrap();
        hooks
    }
    fn names(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}
async fn execute_logged(graph: &Graph, ctx: &mut SystemContext<'_>, log: &ExecutionLog) {
    let validation = graph.validate();
    assert!(validation.is_ok(), "invalid graph: {validation}");
    GraphExecutor::new()
        .execute(graph, ctx, Some(&log.hooks()), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn agent_execution_chooses_granted_declined_or_disabled_paths() {
    for (case, expected) in [
        (
            "granted",
            vec![
                "write.admit",
                "write.reserve",
                "write.stage",
                "write.run",
                "write.settle",
                "write.collect",
            ],
        ),
        (
            "budget",
            vec!["write.admit", "write.reserve", "write.decline"],
        ),
        ("disabled", vec!["write.admit", "write.skip"]),
    ] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls"))
                .unwrap(),
        );
        let mut ctx = context(
            dir.path(),
            runner,
            if case == "budget" { 100 } else { 10_000_000 },
        );
        if case == "disabled" {
            ctx.get_resource::<PhaseControl>()
                .unwrap()
                .interrupt("alpha")
                .unwrap();
        }
        let log = ExecutionLog::default();
        execute_logged(&agent_graph(call("alpha/path")), &mut ctx, &log).await;
        let names = log
            .names()
            .into_iter()
            .filter(|name| name.starts_with("write."))
            .collect::<Vec<_>>();
        assert_eq!(names, expected, "{case}");
        let events = lines(&dir.path().join("events.jsonl"));
        match case {
            "granted" => {
                assert_eq!(events[0]["event"], "agent_start");
                assert_eq!(events[0]["phase"], "alpha");
                assert_eq!(
                    events[0]["fields"],
                    json!({"step":"alpha/path", "role":"worker", "workspace":dir.path().join("work/alpha/path/001")})
                );
                assert_eq!(events[1]["event"], "agent_end");
                assert_eq!(
                    events[1]["fields"],
                    json!({"step":"alpha/path", "role":"worker", "workspace":dir.path().join("work/alpha/path/001"), "stop":"completed", "turns":1, "usd":"0.100000"})
                );
            }
            "budget" => {
                assert_eq!(events[0]["event"], "cap_hit");
                assert_eq!(
                    events[0]["fields"],
                    json!({"cap":"budget.alpha", "limit":100, "used":0, "step":"alpha/path"})
                );
                assert_eq!(events[1]["event"], "step_skipped");
                assert_eq!(
                    events[1]["fields"],
                    json!({"step":"alpha/path", "reason":"budget"})
                );
            }
            "disabled" => assert!(events.iter().all(|event| event["event"] != "agent_start")),
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn repair_loop_passes_immediately_or_after_one_repair_with_visible_execution_order() {
    for needed in [0, 1] {
        let dir = Temp::new();
        let counters =
            RepairCounters::open(dir.path(), BTreeMap::from([("repairs".into(), 3)])).unwrap();
        let mut ctx = SystemContext::new();
        ctx.insert_resource(counters.clone());
        let repaired = Arc::new(AtomicU64::new(0));
        let count = repaired.clone();
        let mut check = Graph::new();
        check.add_system(bind::<(), Verdict, _>("check", move |(), _| {
            let count = count.clone();
            Box::pin(async move {
                Ok(if count.load(Ordering::SeqCst) >= needed {
                    Verdict::Pass
                } else {
                    Verdict::Fail("repair needed".into())
                })
            })
        }));
        let count = repaired.clone();
        let mut repair = Graph::new();
        repair.add_system(bind::<(), (), _>("repair", move |(), _| {
            let count = count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }));
        let graph = repair_loop("verify", "repairs", check, repair);
        let log = ExecutionLog::default();
        execute_logged(&graph, &mut ctx, &log).await;
        let names = log
            .names()
            .into_iter()
            .filter(|name| {
                ["check", "repair", "repair.decide", "repair.consume"].contains(&name.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            if needed == 0 {
                vec!["check", "repair.decide"]
            } else {
                vec![
                    "check",
                    "repair.decide",
                    "repair.consume",
                    "repair",
                    "check",
                    "repair.decide",
                ]
            },
            "repairs needed {needed}"
        );
        assert_eq!(repaired.load(Ordering::SeqCst), needed);
        assert_eq!(counters.used("repairs"), needed as u32);
        assert!(matches!(
            &*ctx.get_output::<Verdict>().unwrap(),
            Verdict::Pass
        ));
    }
}

#[tokio::test]
async fn counter_block_emits_the_updated_count_and_fails_when_exhausted_or_undeclared() {
    let dir = Temp::new();
    let counters =
        RepairCounters::open(dir.path(), BTreeMap::from([("repairs".into(), 1)])).unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(counters.clone());
    assert!(
        counters
            .has_room("missing")
            .unwrap_err()
            .to_string()
            .contains("undeclared counter")
    );
    let result = execute(&counter("repairs"), &mut ctx).await;
    assert_eq!(result.output::<CounterValue>().unwrap().get(), 1);
    for (name, expected) in [
        ("repairs", "counter exhausted"),
        ("missing", "undeclared counter"),
    ] {
        let graph = counter(name);
        assert!(graph.validate().is_ok());
        let error = GraphExecutor::new()
            .execute(&graph, &mut ctx, None, None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{name}: {error}");
    }
    assert_eq!(counters.used("repairs"), 1);
}

#[tokio::test]
async fn budget_block_reads_remaining_allowance_without_reserving_it() {
    let dir = Temp::new();
    let spend = SpendBook::open(
        dir.path(),
        "fixture",
        1_000_000,
        BTreeMap::from([("alpha".into(), 1_000_000)]),
        0,
    )
    .unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(spend.clone());
    for (phase, need, affordable) in [
        ("alpha", 1_000_000, true),
        ("alpha", 1_000_001, false),
        ("missing", 1, false),
    ] {
        let result = execute(&budget(phase, need), &mut ctx).await;
        assert_eq!(
            result.output::<BudgetCheck>().unwrap().is_affordable(),
            affordable,
            "{phase}/{need}"
        );
    }
    assert_eq!(spend.committed(), 0);
    assert_eq!(spend.remaining("alpha"), 1_000_000);
}

#[tokio::test]
async fn events_block_writes_host_fields_and_a_custom_log_filename() {
    let dir = Temp::new();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(
        EventLog::open(dir.path(), "run-id")
            .with_file_name("custom.jsonl")
            .unwrap(),
    );
    execute(
        &events("checkpoint", Some("alpha"), json!({"answer":42})),
        &mut ctx,
    )
    .await;
    let log = lines(&dir.path().join("custom.jsonl"));
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["event"], "checkpoint");
    assert_eq!(log[0]["phase"], "alpha");
    assert_eq!(log[0]["session_id"], "run-id");
    assert_eq!(log[0]["fields"], json!({"answer":42}));
    assert!(!dir.path().join("events.jsonl").exists());
}

#[tokio::test]
async fn rounds_join_every_parallel_body_before_the_barrier_and_next_round() {
    let visits = Arc::new(Mutex::new(Vec::new()));
    let mut body = Graph::new();
    body.add_parallel(
        "workers",
        (0..2)
            .map(|worker| {
                let visits = visits.clone();
                Box::new(move |g: &mut Graph| {
                    g.add_system(move || {
                        let visits = visits.clone();
                        async move {
                            tokio::task::yield_now().await;
                            visits.lock().unwrap().push(worker);
                        }
                    });
                }) as Box<dyn FnOnce(&mut Graph)>
            })
            .collect::<Vec<_>>(),
    );
    let mut barrier = Graph::new();
    let recorded = visits.clone();
    barrier.add_system(move || {
        let recorded = recorded.clone();
        async move {
            let mut recorded = recorded.lock().unwrap();
            let start = recorded
                .len()
                .checked_sub(2)
                .expect("both workers finish before the barrier");
            let mut workers = recorded[start..].to_vec();
            workers.sort();
            assert_eq!(workers, [0, 1]);
            recorded.push(2);
        }
    });
    let graph = rounds("rounds", 3, body, barrier);
    execute(&graph, &mut SystemContext::new()).await;
    assert_eq!(visits.lock().unwrap().len(), 9);
}

#[tokio::test]
async fn fatal_phase_failure_prevents_a_later_enabled_phase_body_from_running() {
    let dir = Temp::new();
    let runner =
        Arc::new(ScriptedRunner::new(Scenario::default(), &dir.path().join("calls")).unwrap());
    let mut ctx = context(dir.path(), runner, 1_000_000);
    let mut body = Graph::new();
    body.add_system(failure);
    let mut graph = phase("alpha", PhasePolicy::Fatal, body);
    let visited = Arc::new(AtomicU64::new(0));
    let count = visited.clone();
    let mut later = Graph::new();
    later.add_system(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
        }
    });
    graph
        .append(phase("beta", PhasePolicy::Fatal, later))
        .unwrap();
    execute(&graph, &mut ctx).await;
    assert_eq!(visited.load(Ordering::SeqCst), 0);
    assert_eq!(
        ctx.get_resource::<PhaseControl>().unwrap().status("beta"),
        None
    );
    let events = lines(&dir.path().join("events.jsonl"));
    assert!(events.iter().all(|event| event["phase"] != "beta"));
}

#[tokio::test]
async fn review_and_revision_phases_run_only_when_explicitly_enabled() {
    for enabled in [false, true] {
        let dir = Temp::new();
        let mut ctx = SystemContext::new();
        ctx.insert_resource(
            PhaseControl::open(
                dir.path(),
                if enabled {
                    vec!["review".into(), "revision".into()]
                } else {
                    vec![]
                },
                BTreeMap::new(),
            )
            .unwrap(),
        );
        ctx.insert_resource(EventLog::open(dir.path(), "fixture"));
        let visits = Arc::new(AtomicU64::new(0));
        for name in ["review", "revision"] {
            let count = visits.clone();
            let mut body = Graph::new();
            body.add_system(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            });
            execute(&phase(name, PhasePolicy::Fatal, body), &mut ctx).await;
            assert_eq!(
                ctx.get_resource::<PhaseControl>().unwrap().status(name),
                Some(if enabled {
                    PhaseStatus::Completed
                } else {
                    PhaseStatus::Skipped("disabled".into())
                })
            );
        }
        assert_eq!(visits.load(Ordering::SeqCst), if enabled { 2 } else { 0 });
    }
}

#[tokio::test]
async fn agent_reports_a_missing_spend_book_resource_by_name() {
    let dir = Temp::new();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(RunHandle {
        run_id: "fixture".into(),
        root: dir.path().into(),
        mode: RunMode::Run,
    });
    ctx.insert_resource(StepJournal::open(dir.path()).unwrap());
    ctx.insert_resource(
        PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap(),
    );
    ctx.insert_resource(EventLog::open(dir.path(), "fixture"));
    ctx.insert_resource(RoleBook {
        roles: BTreeMap::from([("worker".into(), role())]),
    });
    ctx.insert_resource(FlowSettings::default());
    ctx.insert_resource(AgentRunner::new(Arc::new(
        ScriptedRunner::new(Scenario::default(), &dir.path().join("calls")).unwrap(),
    )));
    let graph = agent_graph(call("alpha/missing"));
    assert!(graph.validate().is_ok());
    let error = GraphExecutor::new()
        .execute(&graph, &mut ctx, None, None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SpendBook"), "{error}");
    assert!(lines(&dir.path().join("calls/calls.jsonl")).is_empty());
}
