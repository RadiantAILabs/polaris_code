//! Exercise the shipped CLI against a local Chat Completions endpoint.
//!
//! One scripted server stands in for the model. The tests run the real binary
//! against it and check what the server received, what the tools did on disk,
//! what the process printed and exited with, and what the ATIF trajectory file
//! recorded.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use polaris::models::llm::{LlmRequest, Message, ToolResultContent, ToolResultStatus, UserBlock};
use serde_json::{Value, json};

/// How long the mock server waits for the bytes of one request.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the CLI may run before the test calls it hung.
const CLI_TIMEOUT: Duration = Duration::from_secs(40);
/// How long the mock server keeps accepting before it gives up.
const SERVER_DEADLINE: Duration = Duration::from_secs(45);

const MODEL: &str = "vendor/model:route";
/// A key shaped like an issued `OpenAI` key, so the redactor recognises it. It
/// is the CLI's own key and also the content of a file the shell prints.
const SECRET: &str = "sk-proj-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn4Oo5Pp6";
/// What the redactor writes in place of [`SECRET`].
const REDACTED: &str = "[REDACTED:openai-api-key]";

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let route = headers.lines().next().unwrap().to_string();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() - header_end < length {
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before body");
        bytes.extend_from_slice(&buffer[..count]);
    }
    (
        route,
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    )
}

/// The answer to the first request: five tool calls, whose indexes arrive out
/// of order on purpose so the client has to sort them. The shell call also
/// prints a file that holds [`SECRET`].
fn serve_tool_calls(stream: &mut TcpStream, outside: &Path) {
    let delta = json!({"reasoning_content": "Create the file and verify the shell.", "tool_calls": [
        {"index": 0, "id": "create-1", "type": "function", "function": {"name": "create_file", "arguments": json!({"path": "created.txt", "content": "created\n"}).to_string()}},
        {"index": 2, "id": "outside-create", "type": "function", "function": {"name": "create_file", "arguments": json!({"path": outside.join("new.txt"), "content": "external\n"}).to_string()}},
        {"index": 3, "id": "outside-edit", "type": "function", "function": {"name": "edit_file", "arguments": json!({"path": "../outside/existing.txt", "old_string": "before", "new_string": "after", "replace_all": false}).to_string()}},
        {"index": 4, "id": "outside-read", "type": "function", "function": {"name": "read_file", "arguments": json!({"path": outside.join("existing.txt")}).to_string()}},
        {"index": 1, "id": "shell-1", "type": "function", "function": {"name": "run_command", "arguments": json!({"command": r#"for n in 1 2; do echo "$(printf %s "$n")"; done > shell.txt; cat credentials.env; rm -rf disposable"#}).to_string()}}
    ]});
    stream_reply(stream, delta, "tool_calls");
}

/// The answer to the second request: the final text.
fn serve_final_answer(stream: &mut TcpStream) {
    stream_reply(
        stream,
        json!({"content": "done", "reasoning_content": "Verified."}),
        "stop",
    );
}

/// One Chat Completions event stream with fixed usage, so totals are exact.
fn stream_reply(stream: &mut TcpStream, delta: Value, finish_reason: &str) {
    let chunk = json!({"id": "reply", "model": MODEL, "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}]});
    let usage = json!({"id": "reply", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110, "prompt_tokens_details": {"cached_tokens": 20}, "completion_tokens_details": {"reasoning_tokens": 3}}});
    let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

/// A reply that calls no tool and only says what it will do next.
fn serve_pause(stream: &mut TcpStream) {
    stream_reply(
        stream,
        json!({"content": "Next I will create the file."}),
        "stop",
    );
}

/// A reply the server cut off at the output limit.
fn serve_truncated(stream: &mut TcpStream) {
    stream_reply(stream, json!({"content": "The answer begins"}), "length");
}

/// A refusal the client reports without retrying.
fn serve_refusal(stream: &mut TcpStream) {
    let body = r#"{"error":{"message":"loopback refusal","type":"invalid_request_error"}}"#;
    write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

/// What the mock server does with each request.
#[derive(Clone, Copy)]
enum Script {
    /// Tool calls first, then the final answer.
    ToolCalls,
    /// Refuse every request.
    Refuse,
    /// Read the request and never answer.
    Hang,
    /// Tool calls first, then read the next request and never answer.
    ToolCallsThenHang,
    /// A reply without a tool call first, then the final answer.
    PauseThenAnswer,
    /// A reply cut off at the output limit first, then the final answer.
    TruncatedThenAnswer,
}

/// A loopback server that follows a [`Script`] and records every request.
struct Mock {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    received: Arc<AtomicUsize>,
    thread: JoinHandle<Vec<(String, Value)>>,
}

impl Mock {
    fn start(script: Script, outside: PathBuf) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let (server_stop, server_received) = (stop.clone(), received.clone());
        let thread = std::thread::spawn(move || {
            let mut requests = Vec::new();
            let deadline = Instant::now() + SERVER_DEADLINE;
            while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // Some platforms hand back a socket that inherits the
                        // listener's non-blocking mode, which would make the
                        // reads below fail with `WouldBlock`.
                        stream.set_nonblocking(false).unwrap();
                        requests.push(read_request(&mut stream));
                        server_received.fetch_add(1, Ordering::Relaxed);
                        match script {
                            Script::ToolCalls if requests.len() == 1 => {
                                serve_tool_calls(&mut stream, &outside)
                            }
                            Script::ToolCalls => serve_final_answer(&mut stream),
                            Script::PauseThenAnswer if requests.len() == 1 => {
                                serve_pause(&mut stream)
                            }
                            Script::TruncatedThenAnswer if requests.len() == 1 => {
                                serve_truncated(&mut stream)
                            }
                            Script::PauseThenAnswer | Script::TruncatedThenAnswer => {
                                serve_final_answer(&mut stream)
                            }
                            Script::Refuse => serve_refusal(&mut stream),
                            Script::ToolCallsThenHang if requests.len() == 1 => {
                                serve_tool_calls(&mut stream, &outside)
                            }
                            Script::Hang | Script::ToolCallsThenHang => {
                                while !server_stop.load(Ordering::Relaxed)
                                    && Instant::now() < deadline
                                {
                                    std::thread::sleep(Duration::from_millis(10));
                                }
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    Err(error) => panic!("local endpoint: {error}"),
                }
            }
            requests
        });
        Self {
            address,
            stop,
            received,
            thread,
        }
    }

    /// Stop the server and return the requests it saw, in order.
    fn finish(self) -> Vec<(String, Value)> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap()
    }
}

/// A scratch workspace, a sibling directory the sandbox must protect, and the
/// place for the trajectory file.
struct Setup {
    dir: tempfile::TempDir,
    workspace: PathBuf,
    outside: PathBuf,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("project");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(workspace.join("disposable")).unwrap();
        std::fs::write(
            workspace.join("credentials.env"),
            format!("OPENAI_API_KEY={SECRET}\n"),
        )
        .unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("existing.txt"), "before\n").unwrap();
        Self {
            dir,
            workspace,
            outside,
        }
    }

    fn trajectory(&self) -> PathBuf {
        self.dir.path().join("trajectory.json")
    }

    /// The CLI as an unattended run: a clean environment, the mock as the
    /// model server, and the trajectory written to `trajectory`.
    fn command(
        &self,
        address: SocketAddr,
        trajectory: &Path,
        dangerous: bool,
    ) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_polaris"));
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("POLARIS_CONFIG_DIR", self.dir.path().join("config"))
            .env("OPENAI_API_KEY", SECRET)
            .env("OPENAI_BASE_URL", format!("http://{address}/v1"))
            // These tests count model calls exactly; the nudge has its own
            // tests below, which set the variable themselves.
            .env("POLARIS_CODE_UNATTENDED_NUDGES", "0")
            .args([
                "-p",
                "Create a file and execute the requested verification.",
                "--provider",
                "openai-chat",
                "--model",
                MODEL,
                "--reasoning-effort",
                "high",
                "--context-window",
                "32768",
                "--max-output-tokens",
                "1024",
                "--max-turns",
                "3",
                "--output-format",
                "json",
                "--trajectory-path",
            ])
            .arg(trajectory)
            .kill_on_drop(true);
        if dangerous {
            command.arg("--dangerously-skip-permissions");
        }
        command
    }

    /// Run the CLI to the end against a fresh mock.
    async fn run(
        &self,
        script: Script,
        trajectory: &Path,
        dangerous: bool,
    ) -> (Output, Vec<(String, Value)>) {
        self.run_with_nudges(script, trajectory, dangerous, "0")
            .await
    }

    /// [`run`](Self::run) with `POLARIS_CODE_UNATTENDED_NUDGES` set to `nudges`.
    async fn run_with_nudges(
        &self,
        script: Script,
        trajectory: &Path,
        dangerous: bool,
        nudges: &str,
    ) -> (Output, Vec<(String, Value)>) {
        let mock = Mock::start(script, self.outside.clone());
        let mut command = self.command(mock.address, trajectory, dangerous);
        command.env("POLARIS_CODE_UNATTENDED_NUDGES", nudges);
        let output = tokio::time::timeout(CLI_TIMEOUT, command.output()).await;
        let requests = mock.finish();
        (output.expect("CLI must terminate").unwrap(), requests)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn read_trace(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn model_calls(trace: &Value) -> Vec<&Value> {
    trace["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["extra"]["kind"] == "model_call")
        .collect()
}

/// Both requests carry the run-wide policy, whatever the model does.
fn assert_wire_policy(requests: &[(String, Value)]) {
    assert_eq!(requests.len(), 2, "one tool turn and one final turn");
    for (index, (route, request)) in requests.iter().enumerate() {
        assert_eq!(
            route, "POST /v1/chat/completions HTTP/1.1",
            "request {index}"
        );
        assert_eq!(request["model"], MODEL, "request {index}");
        assert_eq!(request["reasoning_effort"], "high", "request {index}");
        assert_eq!(request["max_completion_tokens"], 1024, "request {index}");
    }
}

/// The messages of the second request that carry tool results, in order.
fn tool_messages(second: &Value) -> Vec<&Value> {
    second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect()
}

/// The second request replays the model's reasoning and answers every call,
/// in the order the client sorted them by index.
fn assert_tool_turn_replayed(second: &Value) -> Vec<&Value> {
    let assistant = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("the second request replays the assistant turn");
    assert_eq!(
        assistant["reasoning_content"],
        "Create the file and verify the shell."
    );
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 5);
    let tools = tool_messages(second);
    let ids = tools
        .iter()
        .map(|tool| tool["tool_call_id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        ids,
        [
            "create-1",
            "shell-1",
            "outside-create",
            "outside-edit",
            "outside-read"
        ],
        "results follow the call indexes, not the order they were streamed"
    );
    tools
}

/// What the tools did, or were refused, on disk and in their results.
fn assert_permission_outcome(dangerous: bool, setup: &Setup, tools: &[&Value]) {
    let (workspace, outside) = (&setup.workspace, &setup.outside);
    if dangerous {
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
            "created\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("shell.txt")).unwrap(),
            "1\n2\n"
        );
        assert!(
            !workspace.join("disposable").exists(),
            "the shell command removed the directory"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("new.txt")).unwrap(),
            "external\n"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("existing.txt")).unwrap(),
            "after\n"
        );
    } else {
        assert!(!workspace.join("created.txt").exists(), "create is refused");
        assert!(!workspace.join("shell.txt").exists(), "shell is refused");
        assert!(
            workspace.join("disposable").exists(),
            "the shell command did not run"
        );
        assert!(
            !outside.join("new.txt").exists(),
            "outside create is refused"
        );
        assert_eq!(
            std::fs::read_to_string(outside.join("existing.txt")).unwrap(),
            "before\n",
            "outside edit is refused"
        );
        for result in tools {
            let content = result["content"].as_str().unwrap();
            if result["tool_call_id"] == "outside-read" {
                assert!(content.contains("escapes sandbox"), "{content}");
            } else {
                assert!(content.contains("denied"), "{content}");
            }
        }
    }
}

/// A secret that a tool prints must reach neither the model nor the file.
///
/// Only the dangerous run lets the shell print `credentials.env`, so only it
/// can show the redaction marker. Neither run may leak the key anywhere,
/// including the copy the CLI holds in its environment and sends in a header.
fn assert_secret_stays_out(
    dangerous: bool,
    requests: &[(String, Value)],
    tools: &[&Value],
    raw_trajectory: &str,
) {
    for (index, (_, request)) in requests.iter().enumerate() {
        assert!(
            !request.to_string().contains(SECRET),
            "request {index} carries the key"
        );
    }
    assert!(
        !raw_trajectory.contains(SECRET),
        "the trajectory file holds the key"
    );
    if dangerous {
        let shell = tools
            .iter()
            .find(|tool| tool["tool_call_id"] == "shell-1")
            .unwrap();
        assert!(
            shell["content"].as_str().unwrap().contains(REDACTED),
            "the model sees the marker, not the key: {shell}"
        );
        assert!(
            raw_trajectory.contains(REDACTED),
            "the trajectory file records the marker where the key was printed"
        );
    }
}

/// What each exported request must look like next to what the server got.
///
/// The server speaks Chat Completions, so the comparison is by shape: the
/// system prompt, then one wire message per rebuilt message, with every tool
/// result as its own `tool` message carrying the same id and text (failed
/// results get an `Error: ` prefix on the wire).
fn assert_rebuilt_matches_wire(rebuilt: &LlmRequest, wire: &Value, label: &str) {
    let mut roles = Vec::new();
    let mut results = Vec::new();
    if rebuilt.system.is_some() {
        roles.push("system");
    }
    for message in &rebuilt.messages {
        match message {
            Message::Assistant { .. } => roles.push("assistant"),
            Message::User { content } => {
                for block in content {
                    match block {
                        UserBlock::ToolResult(result) => {
                            roles.push("tool");
                            let ToolResultContent::Text(text) = &result.content else {
                                panic!("{label}: a tool result without text");
                            };
                            let id = result.call_id.as_ref().unwrap_or(&result.id);
                            // The Chat Completions wire has no error flag, so
                            // the provider marks a failed result in its text.
                            let text = match result.status {
                                ToolResultStatus::Error => format!("Error: {text}"),
                                _ => text.clone(),
                            };
                            results.push((id.clone(), text));
                        }
                        _ => roles.push("user"),
                    }
                }
            }
        }
    }
    let messages = wire["messages"].as_array().unwrap();
    let wire_roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(wire_roles, roles, "{label}: message roles and count");
    let wire_results = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            (
                message["tool_call_id"].as_str().unwrap().to_string(),
                message["content"].as_str().unwrap().to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(wire_results, results, "{label}: tool results by id");
    assert_eq!(
        json!(rebuilt.reasoning_effort),
        wire["reasoning_effort"],
        "{label}: reasoning effort"
    );
    assert_eq!(
        rebuilt.max_output_tokens.map(std::num::NonZeroU32::get),
        wire["max_completion_tokens"].as_u64().map(|n| n as u32),
        "{label}: output limit"
    );
    assert_eq!(
        rebuilt.tools.as_ref().map(Vec::len),
        wire["tools"].as_array().map(Vec::len),
        "{label}: tool definitions"
    );
}

/// A structural ATIF check: what a reader of the file can rely on without
/// knowing anything about this agent.
fn assert_atif_structure(trace: &Value) {
    const AGENT_ONLY: [&str; 6] = [
        "model_name",
        "reasoning_effort",
        "reasoning_content",
        "tool_calls",
        "metrics",
        "llm_call_count",
    ];
    let steps = trace["steps"].as_array().unwrap();
    assert!(!steps.is_empty(), "a trajectory has steps");
    for (index, step) in steps.iter().enumerate() {
        assert_eq!(step["step_id"], index + 1, "step ids run from 1 in order");
        let source = step["source"].as_str().unwrap();
        assert!(
            ["user", "agent", "system"].contains(&source),
            "step {index} has source {source:?}"
        );
        if source != "agent" {
            for field in AGENT_ONLY {
                assert!(
                    step.get(field).is_none(),
                    "step {index} is a {source} step but has {field}"
                );
            }
        }
        let call_ids = step["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|call| call["tool_call_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        for result in step["observation"]["results"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = result["source_call_id"].as_str() {
                assert!(
                    call_ids.contains(&id),
                    "step {index}: result for {id} has no tool call on the same step"
                );
            }
        }
    }
    assert_eq!(
        trace["final_metrics"]["total_steps"],
        steps.len(),
        "final_metrics counts every step"
    );
}

async fn run(dangerous: bool) {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup.run(Script::ToolCalls, &trajectory, dangerous).await;
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        stderr(&output),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_wire_policy(&requests);
    let tools = assert_tool_turn_replayed(&requests[1].1);
    assert_permission_outcome(dangerous, &setup, &tools);

    let raw = std::fs::read_to_string(&trajectory).unwrap();
    assert_secret_stays_out(dangerous, &requests, &tools, &raw);
    let trace: Value = serde_json::from_str(&raw).unwrap();
    assert_atif_structure(&trace);
    let calls = model_calls(&trace);
    assert_eq!(calls.len(), 2, "one trajectory step per model call");
    assert_eq!(
        calls[0]["reasoning_content"],
        "Create the file and verify the shell."
    );
    assert_eq!(
        calls[0]["observation"]["results"].as_array().unwrap().len(),
        5
    );
    assert_eq!(calls[1]["message"], "done");
    for (call, (_, wire)) in calls.iter().zip(&requests) {
        let step_id = call["step_id"].as_u64().unwrap() as usize;
        let rebuilt = polaris_code::trajectory::rebuild_request(&trace, step_id).unwrap();
        assert_rebuilt_matches_wire(&rebuilt, wire, &format!("step {step_id}"));
        assert!(!call["extra"].as_object().unwrap().contains_key("request"));
    }
    assert_eq!(trace["final_metrics"]["total_prompt_tokens"], 200);
    assert_eq!(trace["final_metrics"]["total_completion_tokens"], 20);
    assert_eq!(trace["final_metrics"]["total_cached_tokens"], 40);
    assert_eq!(trace["extra"]["status"], "completed");
}

#[tokio::test]
async fn explicit_container_permissions_run_real_tools_and_export_complete_atif() {
    run(true).await;
}

#[tokio::test]
async fn ordinary_print_remains_fail_closed_at_the_same_endpoint() {
    run(false).await;
}

/// A destination the exporter must refuse stops the run before any model
/// request, with a non-zero exit and a message naming the trajectory.
async fn assert_refused_before_any_request(setup: &Setup, destination: &Path) {
    let (output, requests) = setup.run(Script::ToolCalls, destination, false).await;
    assert!(!output.status.success(), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("cannot create trajectory"),
        "stderr: {}",
        stderr(&output)
    );
    assert!(
        requests.is_empty(),
        "no request may reach the model first: {requests:?}"
    );
}

#[tokio::test]
async fn an_existing_trajectory_file_is_refused_and_left_alone() {
    let setup = Setup::new();
    let destination = setup.trajectory();
    std::fs::write(&destination, "earlier run\n").unwrap();
    assert_refused_before_any_request(&setup, &destination).await;
    assert_eq!(
        std::fs::read_to_string(&destination).unwrap(),
        "earlier run\n",
        "an earlier trajectory is never overwritten"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_trajectory_path_is_refused_and_its_target_is_not_created() {
    let setup = Setup::new();
    let target = setup.dir.path().join("elsewhere.json");
    let destination = setup.trajectory();
    std::os::unix::fs::symlink(&target, &destination).unwrap();
    assert_refused_before_any_request(&setup, &destination).await;
    assert!(!target.exists(), "the link was not followed");
}

#[tokio::test]
async fn a_trajectory_path_with_no_parent_directory_is_refused() {
    let setup = Setup::new();
    let missing = setup.dir.path().join("missing");
    assert_refused_before_any_request(&setup, &missing.join("trajectory.json")).await;
    assert!(
        !missing.exists(),
        "the parent is not created for the caller"
    );
}

#[tokio::test]
async fn a_failed_run_is_finalized_as_failed_with_its_exit_code() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup.run(Script::Refuse, &trajectory, false).await;
    let code = output.status.code().expect("the CLI exits by itself");
    assert_ne!(code, 0, "stderr: {}", stderr(&output));
    assert!(!requests.is_empty(), "the model was asked at least once");
    let trace = read_trace(&trajectory);
    assert_eq!(trace["extra"]["status"], "failed");
    assert_eq!(trace["extra"]["exit_code"], code);
    let calls = model_calls(&trace);
    assert!(!calls.is_empty(), "the refused call is recorded");
    for call in calls {
        assert_eq!(call["extra"]["status"], "failed", "{call}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_a_model_call_still_finalizes_the_trajectory() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let mock = Mock::start(Script::Hang, setup.outside.clone());
    let child = setup
        .command(mock.address, &trajectory, false)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().expect("the CLI is running");
    // The server has the request, so the CLI is waiting on the model and its
    // signal handling is installed.
    let deadline = Instant::now() + CLI_TIMEOUT;
    while mock.received.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "the CLI never asked the model");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let signalled = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(signalled.success(), "kill -TERM failed");
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output()).await;
    mock.finish();
    let output = output.expect("SIGTERM must stop the CLI").unwrap();
    let code = output.status.code().expect("the CLI exits by itself");
    assert_ne!(code, 0, "stderr: {}", stderr(&output));
    let trace = read_trace(&trajectory);
    assert_eq!(trace["extra"]["status"], "failed");
    assert_eq!(trace["extra"]["exit_code"], code);
    let calls = model_calls(&trace);
    assert_eq!(calls.len(), 1, "the interrupted call is recorded");
    assert_ne!(
        calls[0]["extra"]["status"], "completed",
        "the interrupted call is not reported as complete"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_after_tools_ran_still_writes_their_results() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let mock = Mock::start(Script::ToolCallsThenHang, setup.outside.clone());
    let child = setup
        .command(mock.address, &trajectory, false)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id().expect("the CLI is running");
    // The second request means the tools ran and their results are recorded.
    let deadline = Instant::now() + CLI_TIMEOUT;
    while mock.received.load(Ordering::Relaxed) < 2 {
        assert!(
            Instant::now() < deadline,
            "the CLI never asked the model again"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let signalled = std::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .unwrap();
    assert!(signalled.success(), "kill -TERM failed");
    let output = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output()).await;
    mock.finish();
    let output = output.expect("SIGTERM must stop the CLI").unwrap();
    let code = output.status.code().expect("the CLI exits by itself");
    assert_ne!(code, 0, "stderr: {}", stderr(&output));
    let trace = read_trace(&trajectory);
    assert_eq!(trace["extra"]["status"], "failed");
    assert_eq!(trace["extra"]["exit_code"], code);
    let calls = model_calls(&trace);
    assert_eq!(calls.len(), 2, "both calls are recorded");
    let results = calls[0]["observation"]["results"]
        .as_array()
        .expect("the first call's tool results are in the file");
    assert!(!results.is_empty(), "{results:?}");
    assert_ne!(calls[1]["extra"]["status"], "completed");
}

/// The text of the last user message a Chat Completions request carried.
fn last_user_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "user")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn an_unattended_reply_without_a_tool_call_is_asked_to_go_on_once() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup
        .run_with_nudges(Script::PauseThenAnswer, &trajectory, true, "1")
        .await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(requests.len(), 2, "one nudge, then the answer ends the run");
    let nudge = last_user_text(&requests[1].1);
    assert!(nudge.starts_with("[continuation] "), "{nudge}");
    assert!(nudge.contains("the run ends at your next reply"), "{nudge}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("done"),
        "the final answer is the result: {stdout}"
    );
    let trace = read_trace(&trajectory);
    assert_eq!(model_calls(&trace).len(), 2);
    assert_eq!(trace["extra"]["status"], "completed");
}

#[tokio::test]
async fn nudges_set_to_zero_end_the_run_on_the_first_reply_without_a_tool_call() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup
        .run_with_nudges(Script::PauseThenAnswer, &trajectory, true, "0")
        .await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn a_reply_cut_off_at_the_output_limit_is_continued() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup
        .run_with_nudges(Script::TruncatedThenAnswer, &trajectory, true, "0")
        .await;
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(requests.len(), 2, "the cut-off reply is continued once");
    let prompt = last_user_text(&requests[1].1);
    assert!(prompt.starts_with("[continuation] "), "{prompt}");
    assert!(prompt.contains("output limit"), "{prompt}");
    let replayed = requests[1].1["messages"].to_string();
    assert!(
        replayed.contains("The answer begins"),
        "the cut-off text stays in the conversation: {replayed}"
    );
}

#[tokio::test]
async fn a_malformed_nudge_setting_is_a_configuration_error() {
    let setup = Setup::new();
    let trajectory = setup.trajectory();
    let (output, requests) = setup
        .run_with_nudges(Script::PauseThenAnswer, &trajectory, true, "lots")
        .await;
    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(stderr(&output).contains("POLARIS_CODE_UNATTENDED_NUDGES"));
    assert!(requests.is_empty());
}
