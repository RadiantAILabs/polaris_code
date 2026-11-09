//! The command-line surface of the rules setups: what the shipped binary
//! accepts, and what it refuses before it starts anything.
//!
//! Every case here ends at argument or setup validation, ahead of any provider
//! check, so no key, network or model is involved.

use std::process::{Command, Output, Stdio};

fn polaris(env: &[(&str, &str)], args: &[&str]) -> (Output, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("test.py"), "print(1)\n").unwrap();
    let config = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_polaris-guards"));
    command
        .arg(workspace.path())
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", config.path())
        .env("POLARIS_CONFIG_DIR", config.path())
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .stdin(Stdio::null());
    for (key, value) in env {
        command.env(key, value);
    }
    (
        command.output().expect("the polaris binary runs"),
        workspace,
    )
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_per_call_setup_needs_a_protected_path() {
    let (output, _w) = polaris(&[], &["--rules-arm", "per-call", "-p", "x"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("needs at least one --protect path"));
}

#[test]
fn a_steps_setup_needs_a_test_command() {
    let (output, _w) = polaris(
        &[],
        &["--rules-arm", "steps", "--protect", "test.py", "-p", "x"],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("needs --test-command"));
}

#[test]
fn the_moved_setup_parses_in_both_spellings_and_needs_its_inputs() {
    for value in ["moved", "a4", "A4", "Moved"] {
        let (output, _w) = polaris(
            &[("POLARIS_CODE_RULES_ARM", value)],
            &["--protect", "test.py", "-p", "x"],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{value}: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("the moved setup needs --test-command"),
            "{value}: {}",
            stderr(&output)
        );
    }
    let (output, _w) = polaris(&[], &["--rules-arm", "moved", "-p", "x"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("the moved setup needs at least one --protect path"));
}

#[test]
fn the_moved_setup_gets_past_setup_validation_and_writes_its_record() {
    let records = tempfile::tempdir().unwrap();
    let record = records.path().join("record.json");
    let (output, _w) = polaris(
        &[
            ("POLARIS_CODE_RULES_ARM", "a4"),
            ("POLARIS_CODE_PROTECT", "test.py"),
            ("POLARIS_CODE_TEST_COMMAND", "python3 test.py"),
            ("POLARIS_CODE_RUN_RECORD", record.to_str().unwrap()),
        ],
        &["--provider", "anthropic", "-p", "x"],
    );
    // Past setup validation, the next refusal is the provider's: no key.
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("ANTHROPIC_API_KEY"));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(written["arm"], "A4");
    assert_eq!(written["arm_name"], "moved");
    assert_eq!(written["mechanisms"]["prompt_rules"], false);
    assert_eq!(written["mechanisms"]["solution_scan"], true);
}

#[test]
fn a_protected_path_that_does_not_exist_is_refused_at_startup() {
    let (output, _w) = polaris(
        &[],
        &["--rules-arm", "a2", "--protect", "missing.py", "-p", "x"],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("missing.py"));
}

#[test]
fn the_setup_names_and_their_short_aliases_parse_in_either_case_from_the_environment() {
    for value in ["a3", "A3", "steps"] {
        // Parsed as the steps setup: the refusal is the steps setup's own.
        let (output, _w) = polaris(
            &[("POLARIS_CODE_RULES_ARM", value)],
            &["--protect", "test.py", "-p", "x"],
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "{value}: {}",
            stderr(&output)
        );
        assert!(stderr(&output).contains("needs --test-command"), "{value}");
    }
    let (output, _w) = polaris(&[("POLARIS_CODE_RULES_ARM", "a9")], &["-p", "x"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("a9"));
}

#[test]
fn the_inputs_can_come_from_environment_variables() {
    // A per-call setup given its protected path and nothing else gets past
    // setup validation, so the next refusal is the provider's: no key.
    let records = tempfile::tempdir().unwrap();
    let record = records.path().join("record.json");
    let (output, _w) = polaris(
        &[
            ("POLARIS_CODE_RULES_ARM", "a2"),
            ("POLARIS_CODE_PROTECT", "test.py"),
            ("POLARIS_CODE_RUN_RECORD", record.to_str().unwrap()),
        ],
        &["--provider", "anthropic", "-p", "x"],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("ANTHROPIC_API_KEY"),
        "{}",
        stderr(&output)
    );
    // The record exists from the moment the setup was sealed, before any step.
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    assert_eq!(written["arm"], "A2");
    assert_eq!(written["end_reason"], "unfinished");
}

#[test]
fn the_setup_options_need_a_non_interactive_run_and_each_other() {
    let (output, _w) = polaris(&[], &["--rules-arm", "a1"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(stderr(&output).contains("--print"), "{}", stderr(&output));

    let (output, _w) = polaris(&[], &["--protect", "test.py", "-p", "x"]);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("--rules-arm"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn optional_rule_settings_require_an_explicit_arm() {
    for options in [
        vec!["--no-give-up"],
        vec!["--rules-file", "/missing/rules.txt"],
    ] {
        let mut args = vec!["-p", "x"];
        args.extend(options);
        let (output, _w) = polaris(&[], &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("--rules-arm"));
    }
}

#[test]
fn a_missing_rules_file_fails_before_a_provider_is_contacted() {
    let (output, _w) = polaris(
        &[],
        &[
            "-p",
            "x",
            "--rules-arm",
            "none",
            "--rules-file",
            "/missing/rules.txt",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("reading rules file /missing/rules.txt"));
    assert!(!stderr(&output).contains("API key"));
}

#[test]
fn file_rule_words_and_optional_limits_are_recorded_without_interpretation() {
    let files = tempfile::tempdir().unwrap();
    let rules_path = files.path().join("rules.txt");
    let record = files.path().join("record.json");
    let words = "\n**Do not modify tests.**\n\n";
    std::fs::write(&rules_path, words).unwrap();
    let (output, _w) = polaris(
        &[],
        &[
            "-p",
            "x",
            "--rules-arm",
            "none",
            "--no-give-up",
            "--rules-file",
            rules_path.to_str().unwrap(),
            "--max-messages",
            "100",
            "--run-record",
            record.to_str().unwrap(),
            "--provider",
            "anthropic",
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("ANTHROPIC_API_KEY"));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(written["inputs"]["rules_text"], words);
    assert_eq!(written["inputs"]["no_give_up"], true);
    assert_eq!(written["inputs"]["max_messages"], 100);
}

#[test]
fn prompt_gate_requires_protected_paths_and_a_test_command_in_every_spelling() {
    for arm in ["prompt-gate", "Prompt-Gate", "a5", "A5"] {
        let (output, _w) = polaris(&[], &["-p", "x", "--rules-arm", arm]);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            stderr(&output).contains("the prompt-gate setup needs at least one --protect path")
        );
        let (output, _w) = polaris(
            &[],
            &["-p", "x", "--rules-arm", arm, "--protect", "test.py"],
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(stderr(&output).contains("the prompt-gate setup needs --test-command"));
    }
}

#[test]
fn prompt_gate_validates_and_records_its_label_and_mechanisms_before_provider_startup() {
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("record.json");
    let (output, _w) = polaris(
        &[],
        &[
            "-p",
            "x",
            "--provider",
            "anthropic",
            "--rules-arm",
            "prompt-gate",
            "--protect",
            "test.py",
            "--test-command",
            "python3 test.py",
            "--no-give-up",
            "--max-messages",
            "100",
            "--max-holds",
            "9",
            "--run-record",
            record.to_str().unwrap(),
        ],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("ANTHROPIC_API_KEY"));
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(record["arm"], "A5");
    assert_eq!(record["arm_name"], "prompt-gate");
    assert_eq!(record["inputs"]["max_holds"], 9);
    assert_eq!(
        record["mechanisms"],
        serde_json::json!({
            "prompt_rules": true, "per_call_guard": false, "sealed_check": true,
            "done_gate": true, "restore_protected": false, "report_check": true,
            "final_observation": true, "solution_scan": false,
        })
    );
}

#[test]
fn the_binary_without_experiment_flags_uses_the_stock_agent() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (request_tx, request_rx) = mpsc::channel();
    let serving = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut socket = loop {
            match listener.accept() {
                Ok((socket, _)) => break socket,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "local model received no request"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = Vec::new();
        let (offset, length) = loop {
            let mut chunk = [0_u8; 8192];
            let size = socket.read(&mut chunk).unwrap();
            assert!(size > 0);
            bytes.extend_from_slice(&chunk[..size]);
            if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..offset]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                break (offset + 4, length);
            }
        };
        while bytes.len() < offset + length {
            let mut chunk = [0_u8; 8192];
            let size = socket.read(&mut chunk).unwrap();
            assert!(size > 0);
            bytes.extend_from_slice(&chunk[..size]);
        }
        let request: serde_json::Value =
            serde_json::from_slice(&bytes[offset..offset + length]).unwrap();
        request_tx.send(request).unwrap();
        let event = serde_json::json!({
            "id": "local-stock", "object": "chat.completion.chunk", "created": 0, "model": "local-stock",
            "choices": [{"index": 0, "delta": {"content": "stock answer"}, "finish_reason": null}]
        });
        let finish = serde_json::json!({
            "id": "local-stock", "object": "chat.completion.chunk", "created": 0, "model": "local-stock",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        });
        let body = format!("data: {event}\n\ndata: {finish}\n\ndata: [DONE]\n\n");
        write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    let (output, workspace) = polaris(
        &[
            ("OPENAI_API_KEY", "placeholder-never-sent-externally"),
            ("OPENAI_BASE_URL", &endpoint),
        ],
        &[
            "--provider",
            "openai-chat",
            "--model",
            "local-stock",
            "-p",
            "Say hello.",
            "--output-format",
            "json",
        ],
    );
    serving.join().unwrap();
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["result"], "stock answer");
    let request = request_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("precise terminal coding agent"));
    assert!(!system.contains("Rules for this task"));
    assert!(!system.contains("CANNOT_COMPLETE"));
    assert!(
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "edit_file")
    );
    assert!(!workspace.path().join("record.json").exists());
}

#[test]
fn the_binary_accepts_a_message_limit_without_a_rules_arm() {
    let (output, _workspace) = polaris(
        &[("OPENAI_API_KEY", "placeholder-never-sent")],
        &[
            "--provider",
            "openai-chat",
            "--model",
            "local-stock",
            "-p",
            "Say hello.",
            "--max-messages",
            "2",
        ],
    );
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("Message limit reached. count: 2; limit: 2"));
}

#[test]
fn the_original_timeout_variable_keeps_its_bedrock_only_validation() {
    let (output, _workspace) = polaris(
        &[("POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS", "invalid")],
        &["--provider", "anthropic", "-p", "x"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("ANTHROPIC_API_KEY"));
    assert!(!stderr(&output).contains("must be a whole number"));
    let (output, _workspace) = polaris(
        &[
            ("POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS", "invalid"),
            ("AWS_EC2_METADATA_DISABLED", "true"),
            ("AWS_ACCESS_KEY_ID", "placeholder"),
            ("AWS_SECRET_ACCESS_KEY", "placeholder"),
        ],
        &["--provider", "bedrock", "-p", "x"],
    );
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS"));
    assert!(stderr(&output).contains("must be a whole number"));
}
