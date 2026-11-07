//! Exercise the shipped CLI against a local Chat Completions endpoint.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

fn read_request(stream: &mut TcpStream) -> (String, Value) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
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

fn serve(stream: &mut TcpStream, first: bool) {
    let delta = if first {
        json!({"reasoning_content": "Create the file and verify the shell.", "tool_calls": [
            {"index": 0, "id": "create-1", "type": "function", "function": {"name": "create_file", "arguments": json!({"path": "created.txt", "content": "created\n"}).to_string()}},
            {"index": 1, "id": "shell-1", "type": "function", "function": {"name": "run_command", "arguments": json!({"command": r#"for n in 1 2; do echo "$(printf %s "$n")"; done > shell.txt; rm -rf disposable"#}).to_string()}}
        ]})
    } else {
        json!({"content": "done", "reasoning_content": "Verified."})
    };
    let chunk = json!({"id": "reply", "model": "vendor/model:route", "choices": [{"index": 0, "delta": delta, "finish_reason": if first { "tool_calls" } else { "stop" }}]});
    let usage = json!({"id": "reply", "choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110, "prompt_tokens_details": {"cached_tokens": 20}, "completion_tokens_details": {"reasoning_tokens": 3}}});
    let body = format!("data: {chunk}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

async fn run(dangerous: bool) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("project");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(workspace.join("disposable")).unwrap();
    let trajectory = dir.path().join("trajectory.json");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let server = std::thread::spawn(move || {
        let mut requests = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(45);
        while !server_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    requests.push(read_request(&mut stream));
                    serve(&mut stream, requests.len() == 1);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("local endpoint: {error}"),
            }
        }
        requests
    });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_polaris"));
    command
        .current_dir(&workspace)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("POLARIS_CONFIG_DIR", dir.path().join("config"))
        .env("OPENAI_API_KEY", "local-test-placeholder")
        .env("OPENAI_BASE_URL", format!("http://{address}/v1"))
        .args([
            "-p",
            "Create a file and execute the requested verification.",
            "--provider",
            "openai-chat",
            "--model",
            "vendor/model:route",
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
        .arg(&trajectory)
        .kill_on_drop(true);
    if dangerous {
        command.arg("--dangerously-skip-permissions");
    }
    let output = tokio::time::timeout(Duration::from_secs(40), command.output()).await;
    stop.store(true, Ordering::Relaxed);
    let requests = server.join().unwrap();
    let output = output.expect("CLI must terminate").unwrap();
    assert!(
        output.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(requests.len(), 2);
    for (route, request) in &requests {
        assert_eq!(route, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(request["model"], "vendor/model:route");
        assert_eq!(request["reasoning_effort"], "high");
        assert_eq!(request["max_completion_tokens"], 1024);
    }
    let tools = requests[1].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["tool_call_id"], "create-1");
    assert_eq!(tools[1]["tool_call_id"], "shell-1");
    if dangerous {
        assert_eq!(
            std::fs::read_to_string(workspace.join("created.txt")).unwrap(),
            "created\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("shell.txt")).unwrap(),
            "1\n2\n"
        );
        assert!(!workspace.join("disposable").exists());
    } else {
        assert!(!workspace.join("created.txt").exists());
        assert!(!workspace.join("shell.txt").exists());
        assert!(workspace.join("disposable").exists());
        assert!(
            tools
                .iter()
                .all(|result| result["content"].as_str().unwrap().contains("denied"))
        );
    }
    let raw = std::fs::read_to_string(&trajectory).unwrap();
    assert!(!raw.contains("local-test-placeholder"));
    let trace: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(trace["steps"].as_array().unwrap().len(), 3);
    assert_eq!(
        trace["steps"][1]["reasoning_content"],
        "Create the file and verify the shell."
    );
    assert_eq!(
        trace["steps"][1]["observation"]["results"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(trace["steps"][2]["message"], "done");
    assert_eq!(trace["final_metrics"]["total_prompt_tokens"], 200);
    assert_eq!(trace["final_metrics"]["total_completion_tokens"], 20);
    assert_eq!(trace["final_metrics"]["total_cached_tokens"], 40);
    assert_eq!(trace["extra"]["status"], "completed");
    assert!(trace["steps"][1]["extra"]["request"]["system"].is_string());
}

#[tokio::test]
async fn explicit_container_permissions_run_real_tools_and_export_complete_atif() {
    run(true).await;
}

#[tokio::test]
async fn ordinary_print_remains_fail_closed_at_the_same_endpoint() {
    run(false).await;
}
