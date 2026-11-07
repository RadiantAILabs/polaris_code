use super::*;

fn resume(ctx: &mut SystemContext<'_>, root: &Path) {
    ctx.insert_resource(RunHandle {
        run_id: "fixture".into(),
        root: root.into(),
        mode: RunMode::Resume,
    });
}

fn global_context(ctx: SystemContext<'_>) -> SystemContext<'static> {
    let mut globals = polaris::system::resource::Resources::new();
    globals.insert((*ctx.get_resource::<RunHandle>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<RoleBook>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<AgentRunner>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<FlowSettings>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<SpendBook>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<StepJournal>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<EventLog>().unwrap()).clone());
    globals.insert((*ctx.get_resource::<PhaseControl>().unwrap()).clone());
    if let Ok(deadline) = ctx.get_resource::<CallDeadline>() {
        globals.insert((*deadline).clone());
    }
    SystemContext::with_globals(Arc::new(globals))
}

#[tokio::test]
async fn resume_restores_completed_call_outputs_and_phase_without_running_their_bodies() {
    let dir = Temp::new();
    let files = dir.path().join("fixture/outputs");
    std::fs::create_dir_all(&files).unwrap();
    std::fs::write(files.join("result.json"), "stored").unwrap();
    let runner = || {
        Arc::new(
            ScriptedRunner::new(
                scenario(ScriptEntry {
                    final_text: "durable answer".into(),
                    files: Some(dir.path().join("fixture")),
                    ..ScriptEntry::default()
                }),
                &dir.path().join("calls"),
            )
            .unwrap(),
        )
    };
    let graph = agent_graph(call("alpha/completed"));
    let visits = Arc::new(AtomicU64::new(0));
    let count = visits.clone();
    let mut body = Graph::new();
    body.add_system(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            PhaseStatus::Degraded("optional evidence absent".into())
        }
    });
    let phase_graph = phase("beta", PhasePolicy::Degrade, body);
    let mut ctx = global_context(context(dir.path(), runner(), 10_000_000));
    let result = execute(&graph, &mut ctx).await;
    let stored = ctx
        .get_resource::<StepJournal>()
        .unwrap()
        .read(&StepKey::new("alpha/completed").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, StepStatus::Done);
    let original = serde_json::to_value(result.output::<AgentOutcome>().unwrap()).unwrap();
    assert_eq!(stored.output, original);
    assert!(
        matches!(result.output::<AgentOutcome>().unwrap(), AgentOutcome::Ran { report, outputs }
        if report.final_text == "durable answer" && outputs[0].bytes == 6)
    );
    execute(&phase_graph, &mut ctx).await;
    let phase_key = StepKey::new("beta/phase.complete").unwrap();
    assert_eq!(
        ctx.get_resource::<StepJournal>()
            .unwrap()
            .get(&phase_key)
            .unwrap()
            .output,
        serde_json::to_value(PhaseStatus::Degraded("optional evidence absent".into())).unwrap()
    );
    drop(ctx);

    let mut ctx = context(dir.path(), runner(), 10_000_000);
    resume(&mut ctx, dir.path());
    // Clear the saved projection to prove phase restoration comes from the journal.
    ctx.insert_resource(
        PhaseControl::open(
            &dir.path().join("fresh-control"),
            vec!["alpha".into(), "beta".into()],
            BTreeMap::new(),
        )
        .unwrap(),
    );
    let mut ctx = global_context(ctx);
    let result = execute(&graph, &mut ctx).await;
    assert_eq!(
        serde_json::to_value(result.output::<AgentOutcome>().unwrap()).unwrap(),
        original
    );
    execute(&phase_graph, &mut ctx).await;
    assert_eq!(visits.load(Ordering::SeqCst), 1);
    assert_eq!(
        ctx.get_resource::<PhaseControl>().unwrap().status("beta"),
        Some(PhaseStatus::Degraded("optional evidence absent".into()))
    );
    let events = lines(&dir.path().join("events.jsonl"));
    assert_eq!(events.last().unwrap()["fields"]["restored"], true);
    assert_eq!(lines(&dir.path().join("calls/calls.jsonl")).len(), 1);
    assert_eq!(
        lines(&dir.path().join("metrics/cost_events.jsonl")).len(),
        1
    );
}

#[tokio::test]
async fn resume_retries_failed_budget_skipped_and_disabled_calls() {
    for case in ["failed", "budget", "disabled"] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(
                scenario(ScriptEntry {
                    fail: (case == "failed").then(|| "setup".into()),
                    ..ScriptEntry::default()
                }),
                &dir.path().join("calls"),
            )
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
        let graph = agent_graph(call("alpha/retry"));
        let result = execute(&graph, &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        assert!(
            matches!(
                (case, &outcome),
                ("failed", AgentOutcome::Failed(AgentFailure::Setup(_)))
                    | ("budget", AgentOutcome::Skipped(SkipReason::Budget))
                    | ("disabled", AgentOutcome::Skipped(SkipReason::Disabled))
            ),
            "{case}: {outcome:?}"
        );
        let journal = (*ctx.get_resource::<StepJournal>().unwrap()).clone();
        assert_eq!(
            journal
                .read(&StepKey::new("alpha/retry").unwrap())
                .unwrap()
                .unwrap()
                .status,
            if case == "failed" {
                StepStatus::Failed
            } else {
                StepStatus::Skipped
            },
            "{case}"
        );
        drop(ctx);
        let runner = Arc::new(
            ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls"))
                .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        resume(&mut ctx, dir.path());
        let result = execute(&graph, &mut ctx).await;
        assert!(
            matches!(
                result.output::<AgentOutcome>().unwrap(),
                AgentOutcome::Ran { .. }
            ),
            "{case}"
        );
        assert_eq!(
            lines(&dir.path().join("calls/calls.jsonl")).len(),
            if case == "failed" { 2 } else { 1 },
            "{case}"
        );
        assert_eq!(
            ctx.get_resource::<StepJournal>()
                .unwrap()
                .read(&StepKey::new("alpha/retry").unwrap())
                .unwrap()
                .unwrap()
                .status,
            StepStatus::Done,
            "{case}"
        );
    }
}

fn book(root: &Path, limit: u64) -> FlowResult<SpendBook> {
    SpendBook::open(
        root,
        "fixture",
        limit,
        BTreeMap::from([("alpha".into(), 1_000_000), ("beta".into(), 1_000_000)]),
        250_000,
    )
}

#[test]
fn settling_twice_is_a_noop_and_reopening_repairs_missing_or_torn_ledger_lines() {
    for tail in ["", "{\"event_id\":\"torn"] {
        let dir = Temp::new();
        let spend = book(dir.path(), 10_000_000).unwrap();
        let res = spend.reserve("alpha", "worker", 1_000_000).unwrap();
        let usage = Usage {
            model_id: "model".into(),
            micros: 100_000,
            ..Usage::default()
        };
        spend.settle(&res, &usage, StopCause::Finished).unwrap();
        let second = spend.reserve("beta", "other", 1_000_000).unwrap();
        spend
            .settle(
                &second,
                &Usage {
                    micros: 200_000,
                    ..usage.clone()
                },
                StopCause::Finished,
            )
            .unwrap();
        let ledger_path = dir.path().join("metrics/cost_events.jsonl");
        let original = std::fs::read(&ledger_path).unwrap();
        spend
            .settle_status(
                &res,
                &Usage {
                    micros: 900_000,
                    ..usage
                },
                "failed",
            )
            .unwrap();
        assert_eq!(
            std::fs::read(&ledger_path).unwrap(),
            original,
            "tail {tail:?}"
        );
        assert_eq!(spend.committed(), 300_000);
        drop(spend);
        let prefix_end = original.iter().position(|byte| *byte == b'\n').unwrap() + 1;
        let mut damaged = original[..prefix_end].to_vec();
        damaged.extend_from_slice(tail.as_bytes());
        std::fs::write(&ledger_path, damaged).unwrap();
        let reopened = book(dir.path(), 10_000_000).unwrap();
        assert_eq!(
            std::fs::read(&ledger_path).unwrap(),
            original,
            "tail {tail:?}"
        );
        assert_eq!(reopened.committed(), 300_000);
        assert_eq!(reopened.remaining("alpha"), 9_700_000);
        drop(reopened);
        book(dir.path(), 10_000_000).unwrap();
        assert_eq!(
            lines(&ledger_path).len(),
            2,
            "second reopen must not duplicate either settlement"
        );
    }
}

#[test]
fn summary_groups_measured_spend_and_ledger_uses_host_labels_and_usage_provider() {
    let dir = Temp::new();
    let spend = book(dir.path(), 10_000_000)
        .unwrap()
        .with_ledger(LedgerSettings {
            schema_version: 7,
            agent: "researcher".into(),
            provider: "host-provider".into(),
            cost_basis: "metered".into(),
        });
    for (phase, model, provider, micros) in [
        ("alpha", "model-a", "", 100_000),
        ("beta", "model-b", "usage-provider", 200_000),
    ] {
        let res = spend.reserve(phase, "worker", 1_000_000).unwrap();
        spend
            .settle(
                &res,
                &Usage {
                    model_id: model.into(),
                    provider: provider.into(),
                    micros,
                    input: 10,
                    output: 2,
                    ..Usage::default()
                },
                StopCause::Finished,
            )
            .unwrap();
    }
    let pending = spend.reserve("alpha", "pending", 500_000).unwrap();
    let summary = spend.summary().unwrap();
    assert_eq!(summary.total_cost_usd.micros(), 300_000);
    assert_eq!(summary.reserved_usd.micros(), 500_000);
    assert_eq!(summary.cost_basis, "metered");
    assert_eq!(
        summary.models_called,
        BTreeMap::from([("model-a".into(), 1), ("model-b".into(), 1)])
    );
    assert_eq!(summary.by_model.len(), 2);
    assert_eq!(summary.by_phase.len(), 2);
    for (phase, model, micros) in [("alpha", "model-a", 100_000), ("beta", "model-b", 200_000)] {
        assert_eq!(summary.by_model[model].event_count, 1);
        assert_eq!(summary.by_model[model].total_cost_usd.micros(), micros);
        assert_eq!(summary.by_phase[phase], summary.by_model[model]);
    }
    let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
    for line in &ledger {
        assert_eq!(line["schema_version"], 7);
        assert_eq!(line["agent"], "researcher");
        assert_eq!(line["cost_basis"], "metered");
        assert_eq!(line["usage"]["input_tokens"], 10);
        assert_eq!(line["usage"]["output_tokens"], 2);
    }
    assert_eq!(ledger[0]["provider"], "host-provider");
    assert_eq!(ledger[1]["provider"], "usage-provider");
    spend.release(pending).unwrap();
    assert_eq!(spend.summary().unwrap().reserved_usd.micros(), 0);
}

#[test]
fn lower_limits_unknown_phases_and_invalid_fractions_are_refused() {
    let dir = Temp::new();
    let spend = book(dir.path(), 1_000_000).unwrap();
    assert_eq!(spend.phase_cap("unknown"), 0);
    assert_eq!(spend.remaining("unknown"), 0);
    assert!(
        matches!(spend.reserve("unknown", "worker", 100), Err(ReserveError::UnknownPhase(phase)) if phase == "unknown")
    );
    let error = book(dir.path(), 999_999).unwrap_err();
    assert!(
        error.to_string().contains("cannot lower the spend limit"),
        "{error}"
    );
    for (share, minimum) in [(1_000_001, 0), (1_000_000, 1_000_001)] {
        let error = SpendBook::open(
            dir.path(),
            "fixture",
            1_000_000,
            BTreeMap::from([("alpha".into(), share)]),
            minimum,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("invalid budget fraction"),
            "{share}/{minimum}: {error}"
        );
    }
}

#[tokio::test]
async fn an_expired_call_deadline_skips_without_spending_or_starting_the_runner() {
    let dir = Temp::new();
    let runner = Arc::new(
        ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls")).unwrap(),
    );
    let mut ctx = context(dir.path(), runner, 10_000_000);
    ctx.insert_resource(CallDeadline::with_elapsed(
        Duration::from_secs(5),
        Duration::from_secs(10),
    ));
    let mut ctx = global_context(ctx);
    let result = execute(&agent_graph(call("alpha/deadline")), &mut ctx).await;
    assert!(
        matches!(
            result.output::<AgentOutcome>().unwrap(),
            AgentOutcome::Skipped(SkipReason::Budget)
        ),
        "{:?}",
        result.output::<AgentOutcome>()
    );
    assert!(lines(&dir.path().join("calls/calls.jsonl")).is_empty());
    assert_eq!(ctx.get_resource::<SpendBook>().unwrap().committed(), 0);
    let events = lines(&dir.path().join("events.jsonl"));
    assert_eq!(events[0]["event"], "cap_hit");
    assert_eq!(events[0]["fields"]["cap"], "wall_seconds");
    assert_eq!(events[0]["fields"]["limit"], 5);
    assert!(events[0]["fields"]["used"].as_u64().unwrap() >= 10);
    assert_eq!(events[1]["event"], "step_skipped");
    assert_eq!(
        events[1]["fields"],
        json!({"step":"alpha/deadline", "reason":"budget"})
    );
    assert!(!CallDeadline::new(None).expired());
}

#[tokio::test]
async fn persistence_faults_fail_without_budget_events_or_leaked_reservations() {
    for fault in ["spend", "journal"] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls"))
                .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        let path = dir.path().join(if fault == "spend" {
            "metrics/spend_state.json"
        } else {
            "journal.jsonl"
        });
        if path.exists() {
            std::fs::remove_file(&path).unwrap();
        }
        std::fs::create_dir(&path).unwrap();
        let result = execute(&agent_graph(call("alpha/fault")), &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        assert!(
            matches!(&outcome, AgentOutcome::Failed(AgentFailure::Internal(_))),
            "{fault}: {outcome:?}"
        );
        assert!(
            lines(&dir.path().join("calls/calls.jsonl")).is_empty(),
            "{fault}"
        );
        assert!(
            lines(&dir.path().join("metrics/cost_events.jsonl")).is_empty(),
            "{fault}"
        );
        assert_eq!(
            ctx.get_resource::<SpendBook>().unwrap().committed(),
            0,
            "{fault}"
        );
        assert!(
            lines(&dir.path().join("events.jsonl"))
                .iter()
                .all(|line| line["event"] != "cap_hit"),
            "{fault}"
        );
    }
}
