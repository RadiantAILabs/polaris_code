//! Offline behavioral proofs for the generic durable flow contracts.
use polaris::{
    graph::{Graph, GraphExecutor},
    system::{
        param::{Out, Res, SystemContext},
        system,
        system::SystemError,
    },
};
use polaris_cosmos::flow::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
mod blocks;
mod containment;
mod durability;
mod loaders;

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "flow-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        fn writable(p: &Path) {
            if p.is_dir() && !p.is_symlink() {
                let mut permissions = std::fs::metadata(p).unwrap().permissions();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    permissions.set_mode(0o700);
                }
                std::fs::set_permissions(p, permissions).unwrap();
                for e in std::fs::read_dir(p).unwrap() {
                    writable(&e.unwrap().path());
                }
            }
        }
        writable(&self.0);
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn lines(path: &Path) -> Vec<Value> {
    if !path.exists() {
        return vec![];
    }
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}
fn role() -> Role {
    Role {
        prompt: "worker".into(),
        node: "write".into(),
        max_turns: 4,
        max_usd: UsdAmount::from_micros(1_000_000),
        timeout_seconds: 30,
        max_output_tokens: None,
        shell: vec!["python *".into()],
        read_only: false,
        skills: vec![],
    }
}
fn call(step: &str) -> AgentCall {
    AgentCall {
        step: StepKey::new(step).unwrap(),
        role: "worker".into(),
        phase: "alpha".into(),
        stage: StageSpec {
            vars: BTreeMap::from([("VALUE".into(), "private".into())]),
            turn_prompt: Some("Write outputs/result.json".into()),
            ..StageSpec::default()
        },
        expect: vec!["outputs/result.json".into()],
    }
}
fn scenario(entry: ScriptEntry) -> Scenario {
    Scenario {
        usage: Usage {
            model_id: "fixture-model".into(),
            input: 100,
            output: 20,
            requests: 1,
            micros: 100_000,
            ..Usage::default()
        },
        roles: BTreeMap::from([("worker".into(), vec![entry])]),
    }
}
fn context(root: &Path, runner: Arc<dyn TaskRunner>, limit: u64) -> SystemContext<'static> {
    context_with_role(root, runner, limit, role())
}
fn context_with_role(
    root: &Path,
    runner: Arc<dyn TaskRunner>,
    limit: u64,
    worker: Role,
) -> SystemContext<'static> {
    std::fs::create_dir_all(root.join("assets/prompts")).unwrap();
    std::fs::write(
        root.join("assets/prompts/worker.md"),
        "Use {{VALUE}} in {{TASK_DIR}}.",
    )
    .unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(RunHandle {
        run_id: "fixture".into(),
        root: root.into(),
        mode: RunMode::Run,
    });
    ctx.insert_resource(RoleBook {
        roles: BTreeMap::from([("worker".into(), worker)]),
    });
    ctx.insert_resource(AgentRunner::new(runner));
    ctx.insert_resource(FlowSettings {
        assets: root.join("assets"),
        task_dir: root.join("task"),
        model_id: "fixture-model".into(),
        ..FlowSettings::default()
    });
    let shares = BTreeMap::from([("alpha".into(), 1_000_000), ("beta".into(), 1_000_000)]);
    ctx.insert_resource(SpendBook::open(root, "fixture", limit, shares, 250_000).unwrap());
    ctx.insert_resource(StepJournal::open(root).unwrap());
    ctx.insert_resource(EventLog::open(root, "fixture"));
    ctx.insert_resource(
        PhaseControl::open(root, vec!["alpha".into(), "beta".into()], BTreeMap::new()).unwrap(),
    );
    ctx
}
fn agent_graph(call: AgentCall) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(move || {
        let call = call.clone();
        async move { call }
    });
    graph.append(agent("write", "worker")).unwrap();
    graph
}
async fn execute(graph: &Graph, ctx: &mut SystemContext<'_>) -> polaris::graph::ExecutionResult {
    let validation = graph.validate();
    assert!(validation.is_ok(), "invalid graph: {validation}");
    GraphExecutor::new()
        .execute(graph, ctx, None, None)
        .await
        .unwrap()
}
#[test]
fn exact_money_and_cumulative_grants_include_inflight() {
    let dir = Temp::new();
    assert_eq!(parse_micros("12.000003").unwrap(), 12_000_003);
    assert!(parse_micros("0.0000001").is_err());
    assert!(parse_micros("NaN").is_err());
    assert_eq!(dollars(1), "0.000001");
    let book = SpendBook::open(
        dir.path(),
        "run",
        10_000_000,
        BTreeMap::from([("early".into(), 300_000), ("late".into(), 570_000)]),
        250_000,
    )
    .unwrap();
    let a = book.reserve("early", "one", 2_000_000).unwrap();
    let b = book.reserve("early", "two", 2_000_000).unwrap();
    assert_eq!(b.granted_micro, 1_000_000);
    assert!(book.reserve("early", "three", 1_000_000).is_err());
    book.settle(
        &a,
        &Usage {
            micros: 500_000,
            ..Usage::default()
        },
        StopCause::Finished,
    )
    .unwrap();
    book.release(b).unwrap();
    assert_eq!(book.remaining("late"), 5_200_000);
    let abandoned = book.reserve("late", "lost", 1_000_000).unwrap();
    drop(book);
    let book = SpendBook::open(dir.path(), "run", 10_000_000, BTreeMap::new(), 250_000).unwrap();
    assert_eq!(book.recover_abandoned().unwrap()[0].id, abandoned.id);
    let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
    assert_eq!(ledger[1]["status"], "abandoned");
    assert_eq!(ledger[1]["total_cost_usd"], "1.000000");
    assert!(book.recover_abandoned().unwrap().is_empty());
    assert_eq!(
        lines(&dir.path().join("metrics/cost_events.jsonl")).len(),
        2
    );
}
#[tokio::test]
async fn agent_stages_exact_requests_collects_and_numbers_folders() {
    let dir = Temp::new();
    let files = dir.path().join("fixture");
    std::fs::create_dir_all(files.join("outputs")).unwrap();
    std::fs::write(files.join("outputs/result.json"), "{\"ok\":true}").unwrap();
    let runner = Arc::new(
        ScriptedRunner::new(
            scenario(ScriptEntry {
                files: Some(files),
                ..ScriptEntry::default()
            }),
            &dir.path().join("calls"),
        )
        .unwrap(),
    );
    let mut ctx = context(dir.path(), runner, 10_000_000);
    let input = dir.path().join("source.txt");
    std::fs::write(&input, "evidence").unwrap();
    let mut job = call("alpha/write");
    job.stage.copy_in.push((input, "source.txt".into()));
    job.stage
        .vars
        .insert("TASK_DIR".into(), "inputs/private".into());
    let graph = agent_graph(job);
    execute(&graph, &mut ctx).await;
    let result = execute(&graph, &mut ctx).await;
    let requests = lines(&dir.path().join("calls/calls.jsonl"));
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]["workspace"],
        dir.path().join("work/alpha/write/001").to_str().unwrap()
    );
    assert_eq!(
        requests[1]["workspace"],
        dir.path().join("work/alpha/write/002").to_str().unwrap()
    );
    assert_eq!(requests[0]["step"], "alpha/write");
    let workspace = PathBuf::from(requests[0]["workspace"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(workspace.join("role_prompt.md")).unwrap(),
        requests[0]["system_prompt"].as_str().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("task_prompt.md")).unwrap(),
        requests[0]["turn_prompt"].as_str().unwrap()
    );
    assert!(
        std::fs::metadata(workspace.join("inputs/source.txt"))
            .unwrap()
            .permissions()
            .readonly()
    );
    assert_eq!(
        requests[0]["system_prompt"],
        "Use private in inputs/private."
    );
    assert_eq!(requests[0]["turn_prompt"], "Write outputs/result.json");
    let outcome = result.output::<AgentOutcome>().unwrap();
    match outcome {
        AgentOutcome::Ran { outputs, .. } => {
            assert_eq!(outputs.len(), 1);
            assert_eq!(outputs[0].sha256, digest(b"{\"ok\":true}"));
            assert_eq!(outputs[0].path, "outputs/result.json");
            assert_eq!(outputs[0].bytes, 11);
        }
        other => panic!("{other:?}"),
    };
    let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
    for key in [
        "schema_version",
        "event_id",
        "invocation_id",
        "timestamp",
        "agent",
        "provider",
        "model",
        "phase",
        "stage",
        "status",
        "usage",
        "cost_basis",
        "total_cost_usd",
        "artifact",
    ] {
        assert!(ledger[0].get(key).is_some(), "{key}");
    }
    assert_eq!(agent_roles(&graph), ["worker".into()].into());
}
#[tokio::test]
async fn a_role_output_limit_reaches_the_request_and_an_absent_one_stays_absent() {
    for (limit, expected) in [(Some(32_000), Some(32_000)), (None, None)] {
        let dir = Temp::new();
        let files = dir.path().join("fixture");
        std::fs::create_dir_all(files.join("outputs")).unwrap();
        std::fs::write(files.join("outputs/result.json"), "{}").unwrap();
        let runner = Arc::new(
            ScriptedRunner::new(
                scenario(ScriptEntry {
                    files: Some(files),
                    ..ScriptEntry::default()
                }),
                &dir.path().join("calls"),
            )
            .unwrap(),
        );
        let worker = Role {
            max_output_tokens: limit,
            ..role()
        };
        let mut ctx = context_with_role(dir.path(), runner, 10_000_000, worker);
        execute(&agent_graph(call("alpha/write")), &mut ctx).await;
        let requests = lines(&dir.path().join("calls/calls.jsonl"));
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["limits"]["max_output_tokens"].as_u64(),
            expected,
            "limit {limit:?}"
        );
    }
}
#[test]
fn a_role_book_reads_the_output_limit_and_writes_nothing_when_it_is_absent() {
    let with: Role = toml::from_str(
        "prompt='w'\nnode='n'\nmax_turns=3\nmax_usd=1.0\ntimeout_seconds=9\nmax_output_tokens=32000\nshell=[]\nread_only=false\nskills=[]\n",
    )
    .unwrap();
    assert_eq!(with.max_output_tokens, Some(32_000));
    let without: Role = toml::from_str(
        "prompt='w'\nnode='n'\nmax_turns=3\nmax_usd=1.0\ntimeout_seconds=9\nshell=[]\nread_only=false\nskills=[]\n",
    )
    .unwrap();
    assert_eq!(without.max_output_tokens, None);
    assert!(
        !toml::to_string(&without)
            .unwrap()
            .contains("max_output_tokens")
    );
}
#[tokio::test]
async fn refused_agent_never_calls_runner() {
    let dir = Temp::new();
    let runner = Arc::new(
        ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls")).unwrap(),
    );
    let mut ctx = context(dir.path(), runner, 100);
    let result = execute(&agent_graph(call("alpha/refused")), &mut ctx).await;
    assert!(matches!(
        result.output::<AgentOutcome>().unwrap(),
        AgentOutcome::Skipped(SkipReason::Budget)
    ));
    assert!(lines(&dir.path().join("calls/calls.jsonl")).is_empty());
}
#[tokio::test]
async fn every_stop_and_failure_settles_with_expected_status() {
    for (stop, status) in [
        ("finished", "completed"),
        ("turn_cap", "turn_cap"),
        ("cost_cap", "cost_cap"),
        ("timeout", "timeout"),
        ("interrupted", "interrupted"),
        ("model_error", "failed"),
    ] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(
                scenario(ScriptEntry {
                    stop: stop.into(),
                    ..ScriptEntry::default()
                }),
                &dir.path().join("calls"),
            )
            .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        execute(&agent_graph(call("alpha/call")), &mut ctx).await;
        let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
        let settlement = ledger
            .first()
            .unwrap_or_else(|| panic!("{stop}: expected a settlement"));
        assert_eq!(settlement["status"], status, "stop {stop}");
        assert_eq!(settlement["total_cost_usd"], "0.100000", "stop {stop}");
        if stop == "interrupted" {
            assert!(ctx.get_resource::<PhaseControl>().unwrap().interrupted());
            assert!(
                ctx.get_resource::<StepJournal>()
                    .unwrap()
                    .get(&StepKey::new("alpha/call").unwrap())
                    .is_none()
            );
        }
    }
    for (failure, cost) in [
        ("setup", "0.000000"),
        ("unmeasured", "1.000000"),
        ("internal", "1.000000"),
    ] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(
                scenario(ScriptEntry {
                    fail: Some(failure.into()),
                    ..ScriptEntry::default()
                }),
                &dir.path().join("calls"),
            )
            .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        let result = execute(&agent_graph(call("alpha/fail")), &mut ctx).await;
        let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
        let settlement = ledger
            .first()
            .unwrap_or_else(|| panic!("{failure}: expected a settlement"));
        assert_eq!(settlement["status"], "failed", "failure {failure}");
        assert_eq!(settlement["total_cost_usd"], cost, "failure {failure}");
        let outcome = result.output::<AgentOutcome>().unwrap();
        assert!(
            match (failure, outcome) {
                ("setup", AgentOutcome::Failed(AgentFailure::Setup(note))) =>
                    note == "scripted setup failure",
                ("unmeasured", AgentOutcome::Failed(AgentFailure::Unmeasured(note))) =>
                    note == "scripted unmeasured failure",
                ("internal", AgentOutcome::Failed(AgentFailure::Internal(note))) =>
                    note == "scripted internal failure",
                _ => false,
            },
            "failure {failure}: {outcome:?}"
        );
    }
}
#[tokio::test]
async fn stop_flag_wakes_all_waiters_and_interrupts_scripted_delay() {
    let flag = StopFlag::default();
    let f1 = flag.clone();
    let f2 = flag.clone();
    let a = tokio::spawn(async move {
        f1.requested().await;
    });
    let b = tokio::spawn(async move {
        f2.requested().await;
    });
    let dir = Temp::new();
    let runner = ScriptedRunner::new(
        scenario(ScriptEntry {
            delay_ms: 30_000,
            ..ScriptEntry::default()
        }),
        dir.path(),
    )
    .unwrap()
    .with_stop(flag.clone());
    let request = AgentRequest {
        step: StepKey::new("alpha/stop").unwrap(),
        role: "worker".into(),
        workspace: dir.path().into(),
        system_prompt: "role".into(),
        turn_prompt: "turn".into(),
        limits: AgentLimits {
            max_turns: 1,
            max_micros: 1_000_000,
            timeout: Duration::from_secs(60),
            max_output_tokens: None,
        },
        grants: ToolGrant::default(),
    };
    let start = Instant::now();
    let (run, ()) = tokio::join!(runner.run(request.clone()), async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while lines(&dir.path().join("calls.jsonl")).is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runner should record its active request");
        flag.request();
    });
    assert_eq!(run.unwrap().stop, StopCause::Interrupted);
    a.await.unwrap();
    b.await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(
        runner.run(request).await.unwrap().stop,
        StopCause::Interrupted
    );
    let calls = lines(&dir.path().join("calls.jsonl"));
    assert_eq!(calls[0]["played"], true);
    assert_eq!(calls[1]["played"], false);
}
#[system]
async fn failure() -> Result<PhaseStatus, SystemError> {
    Err(SystemError::ExecutionError("fixture failure".into()))
}
#[tokio::test]
async fn phase_preserves_body_status_applies_policies_and_skips_disabled() {
    for (policy, expected, halt, incomplete) in [
        (PhasePolicy::Fatal, "failed", true, false),
        (PhasePolicy::Degrade, "degraded", false, false),
        (PhasePolicy::Incomplete, "failed", false, true),
    ] {
        let dir = Temp::new();
        let runner =
            Arc::new(ScriptedRunner::new(Scenario::default(), &dir.path().join("calls")).unwrap());
        let mut ctx = context(dir.path(), runner, 1_000_000);
        let mut body = Graph::new();
        body.add_system(failure);
        execute(&phase("alpha", policy, body), &mut ctx).await;
        let phases = ctx.get_resource::<PhaseControl>().unwrap();
        assert_eq!(
            phase_status(&phases.status("alpha").unwrap()).0,
            expected,
            "policy {policy:?}"
        );
        assert_eq!(phases.halted(), halt, "policy {policy:?}");
        assert_eq!(phases.incomplete(), incomplete, "policy {policy:?}");
        let events = lines(&dir.path().join("events.jsonl"));
        assert_eq!(events[0]["event"], "phase_start", "policy {policy:?}");
        assert_eq!(events[0]["fields"]["current"], 1, "policy {policy:?}");
        assert_eq!(events[0]["fields"]["total"], 2, "policy {policy:?}");
        assert_eq!(
            events[1]["fields"]["return_code"],
            if halt || incomplete { 1 } else { 0 },
            "policy {policy:?}"
        );
        assert_eq!(events[1]["event"], "phase_end", "policy {policy:?}");
        for key in [
            "event",
            "fields",
            "phase",
            "session_id",
            "source",
            "timestamp",
            "worker_index",
        ] {
            assert!(
                events[0].get(key).is_some(),
                "policy {policy:?}, missing {key}"
            );
        }
    }
    let dir = Temp::new();
    let runner =
        Arc::new(ScriptedRunner::new(Scenario::default(), &dir.path().join("calls")).unwrap());
    let mut ctx = context(dir.path(), runner, 1_000_000);
    let mut body = Graph::new();
    body.add_system(|| async { PhaseStatus::Degraded("optional evidence absent".into()) });
    execute(&phase("alpha", PhasePolicy::Degrade, body), &mut ctx).await;
    assert_eq!(
        ctx.get_resource::<PhaseControl>().unwrap().status("alpha"),
        Some(PhaseStatus::Degraded("optional evidence absent".into()))
    );
    execute(
        &phase("disabled", PhasePolicy::Fatal, Graph::new()),
        &mut ctx,
    )
    .await;
    assert_eq!(
        lines(&dir.path().join("events.jsonl")).last().unwrap()["fields"]["status"],
        "skipped"
    );
}
#[system]
async fn root_cause() -> Result<(), SystemError> {
    Err(SystemError::ExecutionError("root cause".into()))
}
#[system]
async fn follow_on() -> Result<(), SystemError> {
    Err(SystemError::ExecutionError("follow-on failure".into()))
}
#[tokio::test]
async fn phase_reports_its_first_failure_not_the_errors_that_follow_it() {
    let dir = Temp::new();
    let runner =
        Arc::new(ScriptedRunner::new(Scenario::default(), &dir.path().join("calls")).unwrap());
    let mut ctx = context(dir.path(), runner, 1_000_000);
    let mut body = Graph::new();
    body.add_system(|| async { true });
    body.add_conditional_branch::<bool, _, _, _>(
        "branch",
        |go| *go,
        |g| {
            g.add_system(root_cause);
        },
        |g| {
            g.add_system(|| async {});
        },
    );
    body.add_system(follow_on);
    execute(&phase("alpha", PhasePolicy::Fatal, body), &mut ctx).await;
    let status = ctx.get_resource::<PhaseControl>().unwrap().status("alpha");
    let Some(PhaseStatus::Failed(note)) = status else {
        panic!("the phase should have failed, not {status:?}");
    };
    assert!(note.contains("root cause"), "the note was: {note}");
    assert!(!note.contains("follow-on"), "the note was: {note}");
}
#[tokio::test]
async fn memo_restores_typed_output_and_never_repeats_a_done_body() {
    let dir = Temp::new();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(StepJournal::open(dir.path()).unwrap());
    let count = Arc::new(AtomicU64::new(0));
    let body_count = count.clone();
    let mut body = Graph::new();
    body.add_system(move || {
        let count = body_count.clone();
        async move { count.fetch_add(1, Ordering::SeqCst) + 1 }
    });
    let graph = memo::<u64>(StepKey::new("alpha/value").unwrap(), body);
    execute(&graph, &mut ctx).await;
    execute(&graph, &mut ctx).await;
    assert_eq!(*ctx.get_output::<u64>().unwrap(), 1);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn shared_counter_caps_two_repair_loops_and_survives_restart() {
    let dir = Temp::new();
    let counters =
        RepairCounters::open(dir.path(), BTreeMap::from([("shared".into(), 2)])).unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(counters);
    let repaired = Arc::new(AtomicU64::new(0));
    let checks = Arc::new(std::sync::Mutex::new([0; 2]));
    let repairs = Arc::new(std::sync::Mutex::new([0; 2]));
    let mut graph = Graph::new();
    for index in 0..2 {
        let mut check = Graph::new();
        let checked = checks.clone();
        check.add_system(move || {
            let checked = checked.clone();
            async move {
                checked.lock().unwrap()[index] += 1;
                Verdict::Fail("again".into())
            }
        });
        let count = repaired.clone();
        let per_loop = repairs.clone();
        let mut repair = Graph::new();
        repair.add_system(move || {
            let count = count.clone();
            let per_loop = per_loop.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                per_loop.lock().unwrap()[index] += 1;
            }
        });
        graph
            .append(repair_loop(
                &format!("loop/{index}"),
                "shared",
                check,
                repair,
            ))
            .unwrap();
    }
    execute(&graph, &mut ctx).await;
    assert_eq!(repaired.load(Ordering::SeqCst), 2);
    assert_eq!(*checks.lock().unwrap(), [3, 1]);
    assert_eq!(*repairs.lock().unwrap(), [2, 0]);
    assert_eq!(
        RepairCounters::open(dir.path(), BTreeMap::from([("shared".into(), 2)]))
            .unwrap()
            .used("shared"),
        2
    );
}
#[tokio::test]
async fn two_lanes_share_total_wait_for_join_and_tolerate_one_failure() {
    let dir = Temp::new();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(LeaseBook::open(dir.path(), 5).unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let items = seen.clone();
    let graph = lease_loop("lanes", 2, move |worker| {
        let seen = items.clone();
        let mut body = Graph::new();
        body.add_system(bind::<Out<LaneLease>, Verdict, _>(
            "lane.body",
            move |current, _| {
                let seen = seen.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    seen.lock()
                        .unwrap()
                        .push((worker, current.lease.as_ref().unwrap().global));
                    Ok(if worker == 0 {
                        Verdict::Fail("one lane failed".into())
                    } else {
                        Verdict::Pass
                    })
                })
            },
        ));
        body
    });
    execute(&graph, &mut ctx).await;
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen.iter().filter(|(worker, _)| *worker == 0).count(), 1);
    let mut ordinals = seen.iter().map(|(_, ordinal)| *ordinal).collect::<Vec<_>>();
    ordinals.sort();
    assert_eq!(ordinals, vec![1, 2, 3, 4, 5]);
    assert_eq!(ctx.get_resource::<LeaseBook>().unwrap().allocated(), 5);
}
#[tokio::test]
async fn worklist_records_failed_item_then_continues_and_restores() {
    let dir = Temp::new();
    let items = (0..3)
        .map(|i| WorkItem {
            key: StepKey::new(format!("alpha/item{i}")).unwrap(),
            value: json!(i),
            status: WorkItemStatus::Pending,
            note: String::new(),
        })
        .collect();
    let list = Worklist::open(&dir.path().join("list.json"), items).unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(list.clone());
    let mut body = Graph::new();
    body.add_system(bind::<Out<CurrentItem>, Verdict, _>(
        "check.item",
        |item, _| {
            Box::pin(async move {
                let value = item.item().unwrap().value.as_u64().unwrap();
                Ok(if value == 1 {
                    Verdict::Fail("fixture rejected".into())
                } else {
                    Verdict::Pass
                })
            })
        },
    ));
    execute(&worklist("queue", body), &mut ctx).await;
    let loaded = Worklist::open(&dir.path().join("list.json"), vec![]).unwrap();
    assert!(loaded.take().is_none());
    assert_eq!(loaded.items()[1].note, "fixture rejected");
    assert_eq!(
        loaded.items().iter().map(|i| i.status).collect::<Vec<_>>(),
        vec![
            WorkItemStatus::Done,
            WorkItemStatus::Failed,
            WorkItemStatus::Done
        ]
    );
}
#[test]
fn strict_prompt_renderer_names_missing_and_unused_variables() {
    let vars = BTreeMap::from([("NAME".into(), "value".into())]);
    assert_eq!(
        render("worker", "{{NAME}}", &vars, &BTreeMap::new()).unwrap(),
        "value"
    );
    assert!(
        render("worker", "{{MISSING}}", &vars, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("role worker: missing placeholder MISSING")
    );
    assert!(
        render("worker", "plain", &vars, &BTreeMap::new())
            .unwrap_err()
            .to_string()
            .contains("unused variable NAME")
    );
}

#[test]
fn skill_templates_share_variable_validation_and_use_short_staged_paths() {
    let dir = Temp::new();
    let assets = dir.path().join("assets");
    let source = assets.join("prompts/group/skills/log");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "Log {{EXTRA}} from {{TASK_DIR}}.").unwrap();
    std::fs::write(assets.join("prompts/worker.md"), "Use {{VALUE}}.").unwrap();
    let mut role = role();
    role.skills = vec!["group/skills/log".into()];
    let mut call = call("alpha/skill");
    call.stage.vars.insert("EXTRA".into(), "evidence".into());
    call.stage
        .vars
        .insert("TASK_DIR".into(), "inputs/private".into());
    let handle = RunHandle {
        run_id: "fixture".into(),
        root: dir.path().into(),
        mode: RunMode::Run,
    };
    let settings = FlowSettings {
        assets,
        task_dir: dir.path().join("task"),
        model_id: "fixture-model".into(),
        ..FlowSettings::default()
    };
    let workspace = dir.path().join("workspace");
    let request = stage(&call, &role, &workspace, 1_000_000, &handle, &settings).unwrap();
    assert_eq!(request.system_prompt, "Use private.");
    let staged = workspace.join("skills/log/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&staged).unwrap(),
        "Log evidence from inputs/private."
    );
    assert!(std::fs::metadata(staged).unwrap().permissions().readonly());
    call.stage.vars.insert("UNUSED".into(), "bad".into());
    assert!(
        stage(
            &call,
            &role,
            &dir.path().join("unused"),
            1_000_000,
            &handle,
            &settings
        )
        .unwrap_err()
        .to_string()
        .contains("unused variable UNUSED")
    );
}

#[tokio::test]
async fn fallible_lane_is_acknowledged_and_its_sibling_drains_remaining_leases() {
    let dir = Temp::new();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(LeaseBook::open(dir.path(), 4).unwrap());
    let finished = Arc::new(AtomicU64::new(0));
    let count = finished.clone();
    let graph = lease_loop("lanes", 2, move |worker| {
        let count = count.clone();
        let mut body = Graph::new();
        body.add_system(bind::<Out<LaneLease>, Verdict, _>(
            "fallible.lane",
            move |_, _| {
                let count = count.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    if worker == 0 {
                        return Err(SystemError::ExecutionError("lane failed".into()));
                    }
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(Verdict::Pass)
                })
            },
        ));
        body
    });
    tokio::time::timeout(Duration::from_secs(2), execute(&graph, &mut ctx))
        .await
        .unwrap();
    assert_eq!(finished.load(Ordering::SeqCst), 3);
    let verdict = (*ctx.get_output::<Verdict>().unwrap()).clone();
    assert!(
        matches!(&verdict, Verdict::Fail(note) if note.contains("lane failed")),
        "{verdict:?}"
    );
    let book = ctx.get_resource::<LeaseBook>().unwrap();
    assert_eq!(book.allocated(), 4);
    assert!(book.take(0).unwrap().is_none());
    assert!(book.take(1).unwrap().is_none());
}
#[tokio::test]
async fn fallible_work_item_is_recorded_before_the_next_item() {
    let dir = Temp::new();
    let list = Worklist::open(
        &dir.path().join("queue.json"),
        (0..3)
            .map(|i| WorkItem {
                key: StepKey::new(format!("item/n{i}")).unwrap(),
                value: json!(i),
                status: WorkItemStatus::Pending,
                note: String::new(),
            })
            .collect(),
    )
    .unwrap();
    let mut ctx = SystemContext::new();
    ctx.insert_resource(list.clone());
    let mut body = Graph::new();
    body.add_system(bind::<(Out<CurrentItem>, Res<Worklist>), Verdict, _>(
        "fallible.item",
        |(item, list), _| {
            Box::pin(async move {
                if item.item().unwrap().value == 2 {
                    let previous = &list.items()[1];
                    assert_eq!(previous.status, WorkItemStatus::Failed);
                    assert!(previous.note.contains("rejected"), "{}", previous.note);
                }
                if item.item().unwrap().value == 1 {
                    return Err(SystemError::ExecutionError("rejected".into()));
                }
                Ok(Verdict::Pass)
            })
        },
    ));
    execute(&worklist("queue", body), &mut ctx).await;
    assert_eq!(
        list.items().iter().map(|i| i.status).collect::<Vec<_>>(),
        vec![
            WorkItemStatus::Done,
            WorkItemStatus::Failed,
            WorkItemStatus::Done
        ]
    );
}

#[test]
fn interpolated_json_is_data_and_dotted_step_names_do_not_collide() {
    let vars = BTreeMap::from([("JSON".into(), "{\"outer\":{\"value\":1}}".into())]);
    assert_eq!(
        render("worker", "Use {{JSON}}", &vars, &BTreeMap::new()).unwrap(),
        "Use {\"outer\":{\"value\":1}}"
    );
    let dir = Temp::new();
    let journal = StepJournal::open(dir.path()).unwrap();
    let edit = StepKey::new("alpha/prog.edit").unwrap();
    let eval = StepKey::new("alpha/prog.eval").unwrap();
    journal.put(&edit, json!("edited")).unwrap();
    journal.put(&eval, json!("evaluated")).unwrap();
    assert_eq!(journal.get(&edit).unwrap().output, "edited");
    assert_eq!(journal.get(&eval).unwrap().output, "evaluated");
}

#[tokio::test]
async fn a_failed_runner_cannot_drop_a_siblings_inflight_call() {
    let dir = Temp::new();
    let runner = Arc::new(
        ScriptedRunner::new(
            Scenario {
                usage: Usage {
                    model_id: "fixture-model".into(),
                    micros: 10_000,
                    ..Usage::default()
                },
                roles: BTreeMap::from([(
                    "worker".into(),
                    vec![
                        ScriptEntry {
                            when_step: Some("bad".into()),
                            fail: Some("unmeasured".into()),
                            ..ScriptEntry::default()
                        },
                        ScriptEntry {
                            when_step: Some("good".into()),
                            delay_ms: 30,
                            ..ScriptEntry::default()
                        },
                    ],
                )]),
            },
            &dir.path().join("calls"),
        )
        .unwrap(),
    );
    let mut ctx = context(dir.path(), runner, 10_000_000);
    let mut graph = Graph::new();
    let arms = ["bad", "good"]
        .into_iter()
        .map(|name| {
            Box::new(move |g: &mut Graph| {
                g.append(agent_graph(call(&format!("alpha/{name}"))))
                    .unwrap();
            }) as Box<dyn FnOnce(&mut Graph)>
        })
        .collect::<Vec<_>>();
    graph.add_parallel("calls", arms);
    execute(&graph, &mut ctx).await;
    let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().any(|line| line["status"] == "failed"));
    assert!(ledger.iter().any(|line| line["status"] == "completed"));
    assert_eq!(lines(&dir.path().join("calls/calls.jsonl")).len(), 2);
}

#[tokio::test]
async fn phase_deadline_bounds_the_next_role_timeout_and_maps_its_status() {
    let dir = Temp::new();
    let runner = Arc::new(
        ScriptedRunner::new(
            scenario(ScriptEntry {
                delay_ms: 30_000,
                ..ScriptEntry::default()
            }),
            &dir.path().join("calls"),
        )
        .unwrap(),
    );
    let mut ctx = context(dir.path(), runner, 10_000_000);
    ctx.insert_resource(
        PhaseControl::open(
            dir.path(),
            vec!["alpha".into()],
            BTreeMap::from([("alpha".into(), Duration::from_secs(2))]),
        )
        .unwrap(),
    );
    let graph = phase(
        "alpha",
        PhasePolicy::Degrade,
        agent_graph(call("alpha/deadline")),
    );
    execute(&graph, &mut ctx).await;
    let ledger = lines(&dir.path().join("metrics/cost_events.jsonl"));
    assert_eq!(
        ledger.first().expect("deadline call should settle")["status"],
        "timeout"
    );
    assert_eq!(
        ctx.get_resource::<PhaseControl>().unwrap().status("alpha"),
        Some(PhaseStatus::Degraded("phase deadline".into()))
    );
    let calls = lines(&dir.path().join("calls/calls.jsonl"));
    let request = calls.first().expect("deadline call should reach runner");
    let timeout: Duration = serde_json::from_value(request["limits"]["timeout"].clone()).unwrap();
    assert!(
        timeout > Duration::ZERO && timeout <= Duration::from_secs(2),
        "timeout {timeout:?}"
    );
}

#[tokio::test]
async fn dynamic_memo_keys_restore_each_item_when_the_graph_restarts() {
    let dir = Temp::new();
    let evaluated = Arc::new(AtomicU64::new(0));
    let position = Arc::new(AtomicU64::new(0));
    let mut graph = Graph::new();
    let index = position.clone();
    let count = evaluated.clone();
    graph.add_loop_n("items", 3, move |g| {
        g.add_system(move || {
            let index = index.clone();
            async move {
                StepKey::new(format!(
                    "alpha/item{}",
                    index.fetch_add(1, Ordering::SeqCst)
                ))
                .unwrap()
            }
        });
        let mut body = Graph::new();
        body.add_system(move || {
            let count = count.clone();
            async move { count.fetch_add(1, Ordering::SeqCst) + 1 }
        });
        g.append(memo_current::<u64>(body)).unwrap();
    });
    for _ in 0..2 {
        position.store(0, Ordering::SeqCst);
        let mut context = SystemContext::new();
        context.insert_resource(StepJournal::open(dir.path()).unwrap());
        execute(&graph, &mut context).await;
    }
    assert_eq!(evaluated.load(Ordering::SeqCst), 3);
    let journal = StepJournal::open(dir.path()).unwrap();
    for ordinal in 0..3 {
        assert_eq!(
            journal
                .get(&StepKey::new(format!("alpha/item{ordinal}")).unwrap())
                .unwrap()
                .output,
            json!(ordinal + 1)
        );
    }
}

fn interruption_context(control: PhaseControl) -> SystemContext<'static> {
    let mut globals = polaris::system::resource::Resources::new();
    globals.insert(control);
    SystemContext::with_globals(Arc::new(globals))
}

#[tokio::test]
async fn interrupted_lane_keeps_its_lease_for_one_completion_after_restart() {
    let dir = Temp::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let visits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let completed = Arc::new(AtomicU64::new(0));
    let control = PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap();
    let mut context = interruption_context(control.clone());
    context.insert_resource(LeaseBook::open(dir.path(), 1).unwrap());
    let graph = lease_loop("workers", 1, {
        let started = started.clone();
        let release = release.clone();
        let visits = visits.clone();
        let completed = completed.clone();
        move |worker| {
            let started = started.clone();
            let release = release.clone();
            let visits = visits.clone();
            let completed = completed.clone();
            let mut body = Graph::new();
            body.add_system(bind::<(Out<LaneLease>, Res<PhaseControl>), Verdict, _>(
                "lane.body",
                move |(current, control), _| {
                    let started = started.clone();
                    let release = release.clone();
                    let visits = visits.clone();
                    let completed = completed.clone();
                    Box::pin(async move {
                        let first = {
                            let mut visits = visits.lock().unwrap();
                            visits.push((worker, current.lease.clone().unwrap()));
                            visits.len() == 1
                        };
                        if first {
                            started.notify_one();
                            release.notified().await;
                        }
                        if control.halt_reason() == Some(HaltReason::Interrupted) {
                            return Ok(Verdict::Fail("interrupted".into()));
                        }
                        completed.fetch_add(1, Ordering::SeqCst);
                        Ok(Verdict::Pass)
                    })
                },
            ));
            body
        }
    });
    tokio::join!(execute(&graph, &mut context), async {
        started.notified().await;
        control.interrupt("alpha").unwrap();
        release.notify_one();
    });
    assert_eq!(completed.load(Ordering::SeqCst), 0);
    assert_eq!(visits.lock().unwrap().len(), 1);
    drop(context);

    let book = LeaseBook::open(dir.path(), 1).unwrap();
    let pending = book.take(0).unwrap();
    let pending = pending.expect("the interrupted lease stays pending");
    assert_eq!((pending.global, pending.local), (1, 1));
    assert_eq!(book.allocated(), 1);
    let mut resumed = interruption_context(
        PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap(),
    );
    resumed.insert_resource(book.clone());
    execute(&graph, &mut resumed).await;
    let visits = visits.lock().unwrap();
    assert_eq!(visits.len(), 2);
    assert_eq!(visits[0], visits[1]);
    assert_eq!(completed.load(Ordering::SeqCst), 1);
    assert_eq!(book.allocated(), 1);
    assert!(book.take(0).unwrap().is_none());
}

#[tokio::test]
async fn interrupted_worklist_keeps_its_active_item_pending_until_restart() {
    let dir = Temp::new();
    let path = dir.path().join("items.json");
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let visits = Arc::new(std::sync::Mutex::new(Vec::new()));
    let completed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let control = PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap();
    let mut context = interruption_context(control.clone());
    context.insert_resource(
        Worklist::open(
            &path,
            ["first", "second"]
                .into_iter()
                .map(|name| WorkItem {
                    key: StepKey::new("alpha").unwrap().child(name).unwrap(),
                    value: Value::Null,
                    status: WorkItemStatus::Pending,
                    note: String::new(),
                })
                .collect(),
        )
        .unwrap(),
    );
    let mut body = Graph::new();
    body.add_system(bind::<(Out<CurrentItem>, Res<PhaseControl>), Verdict, _>(
        "item.body",
        {
            let started = started.clone();
            let release = release.clone();
            let visits = visits.clone();
            let completed = completed.clone();
            move |(current, control), _| {
                let started = started.clone();
                let release = release.clone();
                let visits = visits.clone();
                let completed = completed.clone();
                Box::pin(async move {
                    let key = current.item().unwrap().key.clone();
                    let first = {
                        let mut visits = visits.lock().unwrap();
                        visits.push(key.clone());
                        visits.len() == 1
                    };
                    if first {
                        started.notify_one();
                        release.notified().await;
                    }
                    if control.halt_reason() == Some(HaltReason::Interrupted) {
                        return Ok(Verdict::Fail("interrupted".into()));
                    }
                    completed.lock().unwrap().push(key);
                    Ok(Verdict::Pass)
                })
            }
        },
    ));
    let graph = worklist("items", body);
    tokio::join!(execute(&graph, &mut context), async {
        started.notified().await;
        control.interrupt("alpha").unwrap();
        release.notify_one();
    });
    let first = StepKey::new("alpha/first").unwrap();
    let second = StepKey::new("alpha/second").unwrap();
    assert_eq!(*visits.lock().unwrap(), vec![first.clone()]);
    assert!(completed.lock().unwrap().is_empty());
    assert!(
        context
            .get_output::<CurrentItem>()
            .unwrap()
            .item()
            .is_none()
    );
    drop(context);

    let list = Worklist::open(&path, Vec::new()).unwrap();
    assert!(
        list.items()
            .iter()
            .all(|item| item.status == WorkItemStatus::Pending)
    );
    assert_eq!(list.take().unwrap().key, first);
    let mut resumed = interruption_context(
        PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap(),
    );
    resumed.insert_resource(list.clone());
    execute(&graph, &mut resumed).await;
    assert_eq!(
        *visits.lock().unwrap(),
        vec![first.clone(), first.clone(), second.clone()]
    );
    assert_eq!(*completed.lock().unwrap(), vec![first, second]);
    assert!(
        list.items()
            .iter()
            .all(|item| item.status == WorkItemStatus::Done)
    );
    assert!(list.take().is_none());
}

#[tokio::test]
async fn both_memo_forms_leave_interrupted_outputs_unfinished_until_restart() {
    for dynamic in [false, true] {
        let dir = Temp::new();
        let key = StepKey::new("alpha/check").unwrap();
        let attempts = Arc::new(AtomicU64::new(0));
        let mut body = Graph::new();
        let count = attempts.clone();
        body.add_system(bind::<Res<PhaseControl>, u64, _>(
            "check",
            move |control, _| {
                let count = count.clone();
                Box::pin(async move {
                    let attempt = count.fetch_add(1, Ordering::SeqCst) + 1;
                    if attempt == 1 {
                        control.interrupt("alpha")?;
                    }
                    Ok(attempt)
                })
            },
        ));
        let graph = if dynamic {
            let mut graph = Graph::new();
            let current_key = key.clone();
            graph.add_system(move || {
                let key = current_key.clone();
                async move { key }
            });
            graph.append(memo_current::<u64>(body)).unwrap();
            graph
        } else {
            memo::<u64>(key.clone(), body)
        };
        let journal = StepJournal::open(dir.path()).unwrap();
        let mut interrupted = interruption_context(
            PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap(),
        );
        interrupted.insert_resource(journal.clone());
        execute(&graph, &mut interrupted).await;
        assert!(journal.get(&key).is_none());
        assert_eq!(
            journal.read(&key).unwrap().unwrap().status,
            StepStatus::Failed
        );
        drop(interrupted);
        let mut resumed = interruption_context(
            PhaseControl::open(dir.path(), vec!["alpha".into()], BTreeMap::new()).unwrap(),
        );
        resumed.insert_resource(StepJournal::open(dir.path()).unwrap());
        execute(&graph, &mut resumed).await;
        execute(&graph, &mut resumed).await;
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(journal.get(&key).unwrap().output, json!(2));
        assert_eq!(
            lines(&dir.path().join("journal.jsonl"))
                .iter()
                .filter(|line| line["key"] == key.as_str() && line["state"] == "done")
                .count(),
            1
        );
    }
}
