use super::*;

fn role_text(amount: &str) -> String {
    format!(
        "[roles.worker]\nprompt='worker'\nnode='write'\nmax_turns=4\nmax_usd={amount}\ntimeout_seconds=30\nshell=[]\nread_only=false\nskills=[]\n"
    )
}

#[test]
fn role_book_loads_number_and_string_amounts_and_round_trips_exact_limits() {
    for (amount, micros) in [("0.30", 300_000), ("2", 2_000_000), ("\"1.5\"", 1_500_000)] {
        let dir = Temp::new();
        std::fs::write(dir.path().join("worker.toml"), role_text(amount)).unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "not TOML").unwrap();
        let book = RoleBook::load(dir.path()).unwrap();
        assert_eq!(book.roles.len(), 1, "{amount}");
        assert_eq!(book.roles["worker"].max_micros(), micros, "{amount}");
        assert_eq!(book.roles["worker"].max_output_tokens, None);
        let saved = dir.path().join("saved");
        std::fs::create_dir(&saved).unwrap();
        book.write(&saved.join("roles.toml")).unwrap();
        let text = std::fs::read_to_string(saved.join("roles.toml")).unwrap();
        assert!(
            text.contains(&format!("max_usd = \"{}\"", dollars(micros))),
            "{text}"
        );
        assert_eq!(
            serde_json::to_value(&RoleBook::load(&saved).unwrap().roles["worker"]).unwrap(),
            serde_json::to_value(&book.roles["worker"]).unwrap()
        );
    }
}

#[test]
fn role_book_rejects_zero_limits_negative_amounts_duplicates_and_malformed_tables() {
    for (case, text, expected) in [
        (
            "output tokens",
            format!("{}max_output_tokens=0\n", role_text("1")),
            "max_output_tokens must be positive",
        ),
        (
            "turns",
            role_text("1").replace("max_turns=4", "max_turns=0"),
            "turns and timeout must be positive",
        ),
        (
            "timeout",
            role_text("1").replace("timeout_seconds=30", "timeout_seconds=0"),
            "turns and timeout must be positive",
        ),
        ("negative", role_text("-1"), "role worker"),
        ("missing table", "".into(), "missing roles table"),
        (
            "wrong table",
            "[other]\nvalue=1\n".into(),
            "expected roles table",
        ),
        (
            "unknown field",
            format!("{}unexpected=true\n", role_text("1")),
            "unknown field",
        ),
        ("malformed", "[roles".into(), "worker.toml"),
        ("duplicate", role_text("1"), "duplicate role worker"),
    ] {
        let dir = Temp::new();
        std::fs::write(dir.path().join("worker.toml"), text).unwrap();
        if case == "duplicate" {
            std::fs::write(dir.path().join("duplicate.toml"), role_text("1")).unwrap();
        }
        let error = RoleBook::load(dir.path()).unwrap_err();
        assert!(error.to_string().contains(expected), "{case}: {error}");
    }
}

fn write_fixture(root: &Path, path: &str, contents: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

#[tokio::test]
async fn scenario_load_merges_includes_in_order_and_local_roles_win_with_sorted_entries() {
    let dir = Temp::new();
    write_fixture(
        dir.path(),
        "base/scenario.toml",
        "[default.usage]\nmicros=100\n",
    );
    write_fixture(
        dir.path(),
        "base/roles/inherited/001.toml",
        "final_text='inherited'\n",
    );
    write_fixture(
        dir.path(),
        "base/roles/worker/001.toml",
        "final_text='base'\n",
    );
    write_fixture(
        dir.path(),
        "second/scenario.toml",
        "[default.usage]\nmicros=200\n",
    );
    write_fixture(
        dir.path(),
        "second/roles/inherited/001.toml",
        "final_text='second include wins'\n",
    );
    write_fixture(
        dir.path(),
        "local/scenario.toml",
        "include=['../base','../second']\n[default.usage]\nmicros=300\nmodel_id='fixture-model'\n",
    );
    // Deliberately create entries in reverse order.
    write_fixture(
        dir.path(),
        "local/roles/worker/002.toml",
        "final_text='second local'\n",
    );
    write_fixture(
        dir.path(),
        "local/roles/worker/001.toml",
        "final_text='first local'\n",
    );
    write_fixture(
        dir.path(),
        "local/roles/worker/001/outputs/result.json",
        "fixture artifact",
    );
    write_fixture(dir.path(), "local/roles/worker/ignored.txt", "not TOML");
    let local = dir.path().join("local");
    let loaded = Scenario::load(&local).unwrap();
    assert_eq!(loaded.usage.micros, 300);
    assert_eq!(
        loaded.roles["inherited"][0].final_text,
        "second include wins"
    );
    assert_eq!(
        loaded.roles["worker"]
            .iter()
            .map(|entry| entry.final_text.as_str())
            .collect::<Vec<_>>(),
        ["first local", "second local"]
    );
    assert_eq!(
        loaded.roles["worker"][0].files.as_ref().unwrap(),
        &local.join("roles/worker/001")
    );
    assert!(loaded.roles["worker"][1].files.is_none());
    let runner = Arc::new(ScriptedRunner::load(&local, &dir.path().join("calls")).unwrap());
    let mut ctx = context(dir.path(), runner, 10_000_000);
    let graph = agent_graph(call("alpha/scenario"));
    let result = execute(&graph, &mut ctx).await;
    assert!(
        matches!(result.output::<AgentOutcome>().unwrap(), AgentOutcome::Ran { report, outputs }
        if report.final_text == "first local" && report.usage.micros == 300 && outputs[0].bytes == 16)
    );
    drop(ctx);
    let runner = Arc::new(ScriptedRunner::load(&local, &dir.path().join("calls")).unwrap());
    let mut ctx = context(dir.path(), runner, 10_000_000);
    let result = execute(&graph, &mut ctx).await;
    assert!(
        matches!(result.output::<AgentOutcome>().unwrap(), AgentOutcome::Ran { report, .. } if report.final_text == "second local")
    );
    let calls = lines(&dir.path().join("calls/calls.jsonl"));
    assert_eq!(calls[0]["ordinal"], 1);
    assert_eq!(calls[1]["ordinal"], 2);
}

#[test]
fn scenario_load_rejects_include_cycles_non_path_includes_and_malformed_entries() {
    for (case, scenario, entry, expected) in [
        ("cycle", "include=['.']", None, "scenario include cycle"),
        (
            "include type",
            "include=[1]",
            None,
            "scenario include must be a path",
        ),
        ("malformed scenario", "[", None, "invalid"),
        ("malformed entry", "", Some("unknown=true"), "unknown field"),
    ] {
        let dir = Temp::new();
        write_fixture(dir.path(), "scenario.toml", scenario);
        if let Some(entry) = entry {
            write_fixture(dir.path(), "roles/worker/001.toml", entry);
        }
        let error = Scenario::load(dir.path()).unwrap_err();
        assert!(error.to_string().contains(expected), "{case}: {error}");
    }
}
