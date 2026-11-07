use super::*;

#[test]
fn unsafe_relative_paths_and_reserved_step_segments_are_rejected() {
    for name in [
        "../x",
        "/absolute",
        "a//b",
        "a\\b",
        "",
        ".",
        "a/../b",
        "a/001",
        "007",
        "_abandoned/x",
        "a/_abandoned",
    ] {
        let error = StepKey::new(name).unwrap_err();
        assert!(matches!(error, FlowError::Invalid(_)), "{name:?}: {error}");
        assert!(
            serde_json::from_value::<StepKey>(json!(name)).is_err(),
            "deserialization accepted {name:?}"
        );
    }
    let key = StepKey::new("alpha").unwrap();
    for child in ["../x", "001", "_abandoned", "a//b", ""] {
        assert!(key.child(child).is_err(), "child {child:?}");
    }
}

#[tokio::test]
async fn staging_rejects_unsafe_input_destinations_before_calling_the_runner() {
    for name in ["../x", "/absolute", "a//b", "a\\b", ""] {
        let dir = Temp::new();
        let runner = Arc::new(
            ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls"))
                .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        let source = dir.path().join("source");
        std::fs::write(&source, "private").unwrap();
        let mut job = call("alpha/unsafe-input");
        job.stage.copy_in.push((source, name.into()));
        let result = execute(&agent_graph(job), &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        assert!(
            matches!(&outcome, AgentOutcome::Failed(AgentFailure::Setup(note)) if note.contains("unsafe relative path")),
            "{name:?}: {outcome:?}"
        );
        assert!(
            lines(&dir.path().join("calls/calls.jsonl")).is_empty(),
            "{name:?}"
        );
        assert_eq!(
            ctx.get_resource::<SpendBook>().unwrap().committed(),
            0,
            "{name:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn staging_refuses_symlinks_escaping_parent_directories_and_special_files() {
    use std::os::unix::{fs::symlink, net::UnixListener};
    for kind in ["symlink", "parent", "socket"] {
        let dir = Temp::new();
        let outside = Temp::new();
        std::fs::write(outside.path().join("secret"), "outside secret").unwrap();
        let source = dir.path().join("source");
        let socket = if kind == "socket" {
            Some(UnixListener::bind(&source).unwrap())
        } else {
            None
        };
        let (source, expected) = match kind {
            "symlink" => {
                std::fs::write(dir.path().join("plain"), "inside").unwrap();
                symlink(dir.path().join("plain"), &source).unwrap();
                (source, "symlink input refused")
            }
            "parent" => {
                symlink(outside.path(), &source).unwrap();
                (source.join("secret"), "input outside the allowed roots")
            }
            "socket" => (source, "special file refused during staging"),
            _ => unreachable!(),
        };
        let runner = Arc::new(
            ScriptedRunner::new(scenario(ScriptEntry::default()), &dir.path().join("calls"))
                .unwrap(),
        );
        let mut ctx = context(dir.path(), runner, 10_000_000);
        let mut job = call("alpha/input");
        job.stage.copy_in.push((source, "snapshot".into()));
        let result = execute(&agent_graph(job), &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        assert!(
            matches!(&outcome, AgentOutcome::Failed(AgentFailure::Setup(note)) if note.contains(expected)),
            "{kind}: {outcome:?}"
        );
        assert!(
            !dir.path()
                .join("work/alpha/input/001/inputs/snapshot")
                .exists(),
            "{kind}"
        );
        assert!(
            lines(&dir.path().join("calls/calls.jsonl")).is_empty(),
            "{kind}"
        );
        assert_eq!(
            ctx.get_resource::<SpendBook>().unwrap().committed(),
            0,
            "{kind}"
        );
        drop(socket);
    }
}

#[cfg(unix)]
struct OutputRunner {
    kind: &'static str,
    outside: PathBuf,
}
#[cfg(unix)]
#[async_trait::async_trait]
impl TaskRunner for OutputRunner {
    async fn run(&self, request: AgentRequest) -> Result<AgentReport, AgentFailure> {
        use std::os::unix::fs::symlink;
        let output = request.workspace.join("outputs/result.json");
        match self.kind {
            "symlink" => symlink(self.outside.join("result.json"), output).unwrap(),
            "directory" => std::fs::create_dir(output).unwrap(),
            "outside" => {
                std::fs::remove_dir(request.workspace.join("outputs")).unwrap();
                symlink(&self.outside, request.workspace.join("outputs")).unwrap();
            }
            "oversize" => std::fs::write(output, "12345").unwrap(),
            "missing" => (),
            _ => unreachable!(),
        }
        Ok(AgentReport {
            stop: StopCause::Finished,
            usage: Usage::default(),
            final_text: "complete".into(),
            transcript: None,
        })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn collection_refuses_symlinks_directories_escaped_outputs_and_oversized_files() {
    for (kind, expected) in [
        ("symlink", "expected output must be an ordinary file"),
        ("directory", "expected output must be an ordinary file"),
        ("outside", "output outside call workspace"),
        ("oversize", "over the 4 byte limit"),
    ] {
        let dir = Temp::new();
        let outside = Temp::new();
        std::fs::write(outside.path().join("result.json"), "outside").unwrap();
        let runner = Arc::new(OutputRunner {
            kind,
            outside: outside.path().into(),
        });
        let mut ctx = context(dir.path(), runner, 10_000_000);
        ctx.insert_resource(FlowSettings {
            assets: dir.path().join("assets"),
            task_dir: dir.path().join("task"),
            max_output_bytes: 4,
            ..FlowSettings::default()
        });
        let result = execute(&agent_graph(call("alpha/output")), &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        assert!(
            matches!(&outcome, AgentOutcome::Failed(AgentFailure::Internal(note)) if note.contains(expected)),
            "{kind}: {outcome:?}"
        );
        assert_eq!(
            ctx.get_resource::<StepJournal>()
                .unwrap()
                .read(&StepKey::new("alpha/output").unwrap())
                .unwrap()
                .unwrap()
                .status,
            StepStatus::Failed,
            "{kind}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn absent_declared_outputs_are_optional_but_unsafe_output_paths_fail() {
    for expected in [
        "outputs/result.json",
        "../x",
        "/absolute",
        "a//b",
        "a\\b",
        "",
    ] {
        let dir = Temp::new();
        let runner = Arc::new(OutputRunner {
            kind: "missing",
            outside: dir.path().into(),
        });
        let mut ctx = context(dir.path(), runner, 10_000_000);
        let mut job = call("alpha/optional-output");
        job.expect = vec![expected.into()];
        let result = execute(&agent_graph(job), &mut ctx).await;
        let outcome = result.output::<AgentOutcome>().unwrap().clone();
        if expected == "outputs/result.json" {
            assert!(
                matches!(&outcome, AgentOutcome::Ran { outputs, .. } if outputs.is_empty()),
                "{outcome:?}"
            );
        } else {
            assert!(
                matches!(&outcome, AgentOutcome::Failed(AgentFailure::Internal(note)) if note.contains("unsafe relative path")),
                "{expected:?}: {outcome:?}"
            );
        }
    }
}

#[test]
fn corrupt_journal_and_spend_state_return_errors_without_panicking() {
    let dir = Temp::new();
    let error = RoleBook::load(&dir.path().join("absent")).unwrap_err();
    assert_eq!(error.io_kind(), Some(std::io::ErrorKind::NotFound));
    assert!(std::error::Error::source(&error).is_some());
    let journal = StepJournal::open(dir.path()).unwrap();
    let key = StepKey::new("check").unwrap();
    assert!(journal.read(&key).unwrap().is_none());
    std::fs::write(dir.path().join(".state/steps/check.json"), "{").unwrap();
    assert!(matches!(journal.read(&key), Err(FlowError::Json(_))));
    assert!(journal.get(&key).is_none());
    std::fs::create_dir_all(dir.path().join("metrics")).unwrap();
    std::fs::write(
        dir.path().join("metrics/spend_state.json"),
        r#"{"limit_micro":100,"settlements":{"bad":{}}}"#,
    )
    .unwrap();
    assert!(matches!(
        SpendBook::open(dir.path(), "fixture", 100, BTreeMap::new(), 0),
        Err(FlowError::Json(_))
    ));
}
