use super::*;
use polaris_tui::commands::CommandInvocation;
use serde_json::json;

fn fixture(mode: &str) -> ServerConfig {
    ServerConfig {
        command: Some("python3".into()),
        args: vec![
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src/tools/mcp/fixture.py")
                .display()
                .to_string(),
        ],
        env: BTreeMap::from([
            ("FIXTURE_MODE".into(), mode.into()),
            ("EXPLICIT_MCP_VALUE".into(), "configured".into()),
        ]),
        url: None,
        read_only: false,
        timeout_secs: 1,
        disabled: false,
    }
}

struct FixtureRegistry {
    registry: McpRegistry,
    process_dir: tempfile::TempDir,
}

impl std::ops::Deref for FixtureRegistry {
    type Target = McpRegistry;
    fn deref(&self) -> &Self::Target {
        &self.registry
    }
}

impl Drop for FixtureRegistry {
    fn drop(&mut self) {
        for entry in self.registry.entries.iter() {
            if let Some(connection) = &entry.connection
                && let Ok(service) = connection.service.try_lock()
            {
                service.cancellation_token().cancel();
            }
        }
    }
}

async fn fixture_registry(mode: &str) -> FixtureRegistry {
    let process_dir = tempfile::tempdir().unwrap();
    let mut config = fixture(mode);
    config.env.insert(
        "FIXTURE_PID".into(),
        process_dir.path().join("pid").display().to_string(),
    );
    let registry = discover(
        BTreeMap::from([("local".into(), config)]),
        Vec::new(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .await;
    FixtureRegistry {
        registry,
        process_dir,
    }
}

async fn assert_disconnected_and_child_exited(registry: &FixtureRegistry) {
    let pid = std::fs::read_to_string(registry.process_dir.path().join("pid")).unwrap();
    // Poll observable liveness instead of using a sleep to infer termination.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let alive = tokio::process::Command::new("python3")
                .args([
                    "-c",
                    "import os,sys; os.kill(int(sys.argv[1]), 0)",
                    pid.trim(),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .unwrap()
                .success();
            if registry.servers()[0].status == McpStatus::Disconnected && !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled transport closes and its child exits");
    assert_eq!(registry.servers()[0].status, McpStatus::Disconnected);
}

fn tool<'a>(registry: &'a McpRegistry, remote_name: &str) -> &'a McpTool {
    registry.entries[0]
        .tools
        .iter()
        .find(|tool| tool.remote_name == remote_name)
        .unwrap()
}

async fn close(registry: &McpRegistry) {
    if let Some(connection) = &registry.entries[0].connection {
        connection.close().await;
    }
}

#[tokio::test]
async fn discovery_registers_tools_and_calls_through_framework_registry() {
    let registry = fixture_registry("pagination").await;
    assert_eq!(registry.servers()[0].status, McpStatus::Connected);
    assert_eq!(registry.servers()[0].tools.len(), 7);
    let mut tools = ToolRegistry::new();
    for entry in registry.entries.iter() {
        for tool in &entry.tools {
            assert_eq!(tool.permission(), ToolPermission::Confirm);
            tools.register(tool.clone());
        }
    }
    let definitions = tools.definitions();
    let definition = definitions
        .iter()
        .find(|tool| tool.name == "mcp__local__echo")
        .unwrap();
    assert_eq!(definition.name, "mcp__local__echo");
    assert!(!definition.strict);
    assert_eq!(
        definition.parameters,
        json!({"type": "object", "properties": {}})
    );
    let result = tools
        .execute("mcp__local__echo", &json!({"message": "hello"}))
        .await
        .unwrap();
    assert!(result.as_str().unwrap().contains("hello"));
    close(&registry).await;
}

#[tokio::test]
async fn server_tool_errors_are_errors_and_results_are_bounded() {
    let registry = fixture_registry("").await;
    let ctx = ToolContext::new();
    let error = tool(&registry, "error")
        .execute(json!({"reason": "failure"}), &ctx)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("failure"));
    let result = tool(&registry, "large")
        .execute(json!({}), &ctx)
        .await
        .unwrap();
    let result = result.as_str().unwrap();
    assert!(result.len() <= super::super::code::MAX_READ_BYTES + 100);
    assert!(result.contains("MCP result truncated"));
    // Truncation preserves a UTF-8 boundary.
    assert!(result.contains('é'));
    let invalid = tool(&registry, "echo")
        .execute(json!([]), &ctx)
        .await
        .unwrap_err();
    assert!(invalid.to_string().contains("must be an object"));
    close(&registry).await;
}

#[tokio::test]
async fn mcp_result_uses_the_existing_secret_redaction_format() {
    use crate::agents::code_agent::SecretRedactor;

    let registry = fixture_registry("").await;
    let secret = format!("sk-ant-api03-{}", "a1B2".repeat(20));
    let result = tool(&registry, "echo")
        .execute(json!({"token": secret}), &ToolContext::new())
        .await
        .unwrap();
    let result = result.as_str().unwrap();
    assert!(result.contains(&secret));
    let redacted = SecretRedactor::new(true).scan(result).into_text();
    assert!(!redacted.contains(&secret));
    close(&registry).await;
}

#[tokio::test]
async fn timeout_crash_and_protocol_error_fail_without_private_diagnostics() {
    for name in ["timeout", "crash", "rpc_error"] {
        let registry = fixture_registry("").await;
        let error = tool(&registry, name)
            .execute(json!({}), &ToolContext::new())
            .await
            .unwrap_err();
        let error = error.to_string();
        assert!(!error.contains("private server diagnostic"));
        if name == "timeout" {
            assert!(error.contains("timed out"));
        }
        if name == "rpc_error" {
            let result = tool(&registry, "echo")
                .execute(json!({"after_error": true}), &ToolContext::new())
                .await
                .unwrap();
            assert!(result.as_str().unwrap().contains("after_error"));
        }
        if name != "rpc_error" {
            assert_disconnected_and_child_exited(&registry).await;
        } else {
            assert_eq!(registry.servers()[0].status, McpStatus::Connected);
        }
        close(&registry).await;
    }
}

#[tokio::test]
async fn cancelled_call_disconnects_and_shutdown_is_bounded() {
    let registry = fixture_registry("").await;
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        tool(&registry, "timeout").execute(json!({}), &ToolContext::new()),
    )
    .await;
    assert!(result.is_err());
    assert_disconnected_and_child_exited(&registry).await;
    close(&registry).await;
}

#[tokio::test]
async fn child_environment_is_minimal_with_explicit_values() {
    let registry = fixture_registry("").await;
    let result = tool(&registry, "environment")
        .execute(json!({}), &ToolContext::new())
        .await
        .unwrap();
    let envelope: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
    let environment: BTreeMap<String, String> =
        serde_json::from_str(envelope["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(environment["PATH"], "/usr/local/bin:/usr/bin:/bin");
    assert_eq!(environment["EXPLICIT_MCP_VALUE"], "configured");
    for name in environment.keys() {
        assert!(
            [
                "HOME",
                "USER",
                "LOGNAME",
                "SHELL",
                "LANG",
                "TERM",
                "TMPDIR",
                "PATH",
                "FIXTURE_MODE",
                "FIXTURE_PID",
                "EXPLICIT_MCP_VALUE",
                // Set by the child's own runtime at startup, not inherited:
                // Python's locale coercion and macOS CoreFoundation.
                "LC_CTYPE",
                "__CF_USER_TEXT_ENCODING"
            ]
            .contains(&name.as_str()),
            "unexpected inherited variable: {name}"
        );
    }
    close(&registry).await;
}

#[tokio::test]
async fn discovery_failures_are_visible_without_registering_partial_tools() {
    for (mode, expected) in [
        (
            "bad-name",
            "discovery rejected invalid or duplicate tool names",
        ),
        (
            "duplicate",
            "discovery rejected invalid or duplicate tool names",
        ),
        ("repeated-cursor", "tool discovery repeated a cursor"),
        ("init-timeout", "initialization timed out"),
        ("discovery-timeout", "tool discovery timed out"),
        ("too-many-tools", "tool discovery exceeded 256 tools"),
        ("too-many-pages", "tool discovery exceeded 32 pages"),
        (
            "metadata-limit",
            "tool discovery exceeded 1048576 metadata bytes",
        ),
        (
            "schema-limit",
            "tool discovery exceeded 1048576 metadata bytes",
        ),
    ] {
        let registry = fixture_registry(mode).await;
        let status = &registry.servers()[0];
        assert_eq!(status.status, McpStatus::Failed(expected.into()), "{mode}");
        assert!(status.tools.is_empty(), "{mode}");
        assert!(registry.report().contains(expected), "{mode}");
    }
    let mut config = fixture("");
    config.command = Some("/nonexistent/mcp-fixture".into());
    let registry = discover(
        BTreeMap::from([("missing".into(), config)]),
        Vec::new(),
        Path::new("."),
    )
    .await;
    assert!(registry.report().contains("cannot start server command"));
}

#[tokio::test]
async fn permissions_confirm_plan_and_unattended_grants() {
    use crate::agents::permission_rules::{PermissionCall, ReviewEligibility, SessionGrants};
    use crate::agents::{PermissionMode, UnattendedGrants};
    let mut read_only = fixture("");
    read_only.read_only = true;
    let registry = discover(
        BTreeMap::from([("local".into(), fixture("")), ("ro".into(), read_only)]),
        Vec::new(),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    )
    .await;
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(McpClientPlugin {
            registry: registry.clone(),
        });
    server.finish().await.unwrap();
    let tools = server.get_global::<ToolPolicyRegistry>().unwrap().clone();
    let registered = server.get_global::<ToolRegistry>().unwrap();
    for (name, operator_read_only) in [("mcp__local__echo", false), ("mcp__ro__echo", true)] {
        // A host that follows registry permissions alone still confirms every
        // MCP tool, including the ones an operator declared read-only.
        let default = registered.permission(name).unwrap();
        assert_eq!(default, ToolPermission::Confirm, "{name}");
        for mode in [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Auto,
        ] {
            let mut policy = mode.permission_policy();
            policy.tools = tools.clone();
            assert_eq!(policy.resolve(name, default), ToolPermission::Confirm);
            policy.overrides.insert(name.into(), ToolPermission::Allow);
            assert_eq!(
                policy.resolve(name, default),
                ToolPermission::Confirm,
                "an override cannot waive the prompt in {}",
                mode.label()
            );
            policy.overrides.insert(name.into(), ToolPermission::Deny);
            assert_eq!(policy.resolve(name, default), ToolPermission::Deny);
        }
        let mut plan = PermissionMode::Plan.permission_policy();
        plan.tools = tools.clone();
        assert_eq!(
            plan.resolve(name, default),
            if operator_read_only {
                ToolPermission::Allow
            } else {
                ToolPermission::Deny
            },
            "{name}"
        );
        let mut unattended =
            PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
        unattended.tools = tools.clone();
        assert_eq!(unattended.resolve(name, default), ToolPermission::Deny);
        unattended
            .overrides
            .insert(name.into(), ToolPermission::Allow);
        assert_eq!(unattended.resolve(name, default), ToolPermission::Allow);
        unattended
            .overrides
            .insert(name.into(), ToolPermission::Deny);
        assert_eq!(unattended.resolve(name, default), ToolPermission::Deny);
        // A registry refusal cannot be granted back, even unattended.
        unattended
            .overrides
            .insert(name.into(), ToolPermission::Allow);
        assert_eq!(
            unattended.resolve(name, ToolPermission::Deny),
            ToolPermission::Deny
        );
    }
    // In `auto` the call reaches a person, never the automatic reviewer.
    let mut auto = PermissionMode::Auto.permission_policy();
    auto.tools = tools.clone();
    let args = json!({});
    let decision = auto.resolve_call(
        &PermissionCall {
            tool: "mcp__ro__echo",
            args: &args,
            registry_default: ToolPermission::Confirm,
            root: Path::new("."),
            generation: 1,
            ceiling: None,
        },
        &SessionGrants::default(),
    );
    assert_eq!(decision.permission, ToolPermission::Confirm);
    assert_eq!(decision.review, ReviewEligibility::HumanOnly);
    drop(registered);
    server.cleanup().await;
}

#[tokio::test]
async fn registration_order_does_not_change_what_the_plugin_publishes() {
    for reverse in [false, true] {
        let fixture = fixture_registry("").await;
        let registry = fixture.registry.clone();
        let mut server = Server::new();
        if reverse {
            server
                .add_plugins(McpClientPlugin {
                    registry: registry.clone(),
                })
                .add_plugins(ToolsPlugin);
        } else {
            server
                .add_plugins(ToolsPlugin)
                .add_plugins(McpClientPlugin { registry });
        }
        server.finish().await.unwrap();
        let registered = server.get_global::<ToolRegistry>().unwrap();
        assert_eq!(
            registered.permission("mcp__local__echo"),
            Some(ToolPermission::Confirm),
            "reverse: {reverse}"
        );
        let facts = server.get_global::<ToolPolicyRegistry>().unwrap().clone();
        assert_eq!(
            facts.get("mcp__local__echo").attended(),
            AttendedApproval::Human,
            "reverse: {reverse}"
        );
        drop(registered);
        server.cleanup().await;
    }
}

#[test]
fn excess_server_count_is_rejected_without_starting_processes() {
    let configs = (0..33)
        .map(|index| (format!("server{index}"), fixture("")))
        .collect();
    let registry = tokio::runtime::Runtime::new().unwrap().block_on(discover(
        configs,
        Vec::new(),
        Path::new("."),
    ));
    assert!(registry.servers().is_empty());
    assert!(registry.configuration_errors()[0].contains("32 servers"));
}

#[test]
fn config_precedence_invalid_json_and_name_collisions() {
    let project = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let user_path = user.path().join("mcp.json");
    std::fs::write(&user_path, r#"{"mcpServers":{"same":{"command":"user"},"onlyUser":{"url":"https://example.invalid/mcp"}}}"#).unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        r#"{"mcpServers":{"same":{"command":"project","readOnly":true}}}"#,
    )
    .unwrap();
    let loaded = config::load(project.path(), Some(&user_path), &ProjectTrust::default());
    // The user's server keeps its name and its declaration.
    assert_eq!(loaded.servers["same"].command.as_deref(), Some("user"));
    assert!(!loaded.servers["same"].read_only);
    assert!(loaded.servers.contains_key("onlyUser"));
    assert!(loaded.withheld.is_empty());
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].contains("`same` ignored"));
    std::fs::write(
        project.path().join(".mcp.json"),
        "private credential malformed",
    )
    .unwrap();
    let loaded = config::load(project.path(), Some(&user_path), &ProjectTrust::default());
    assert_eq!(loaded.servers["same"].command.as_deref(), Some("user"));
    assert_eq!(loaded.errors.len(), 1);
    assert!(!loaded.errors[0].contains("credential"));
    for bad in ["", "a__b", "_", "a_", "_a", "../server", "name\n"] {
        assert!(!valid_component(bad));
    }
}

#[cfg(unix)]
#[test]
fn project_config_symlink_cannot_escape_project() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let source = outside.path().join("mcp.json");
    std::fs::write(&source, r#"{"mcpServers":{}}"#).unwrap();
    std::os::unix::fs::symlink(source, project.path().join(".mcp.json")).unwrap();
    let loaded = config::load(project.path(), None, &ProjectTrust::default());
    assert!(loaded.errors[0].contains("escapes"));
}

#[tokio::test]
async fn mcp_command_is_local_and_lists_connection_state() {
    let commands = CommandRegistry::with_builtins();
    let registry = fixture_registry("").await;
    let projection = registry.clone();
    commands
        .register_sync(CommandSpec::new("mcp", "List MCP tools"), move |_, _| {
            CommandOutcome::Reply(projection.report())
        })
        .unwrap();
    let CommandOutcome::Reply(text) = commands.dispatch(&CommandInvocation::new("mcp", "")).await
    else {
        panic!("mcp command replies");
    };
    assert!(text.contains("connected"));
    assert!(text.contains("mcp__local__echo"));
    close(&registry).await;
    let CommandOutcome::Reply(text) = commands.dispatch(&CommandInvocation::new("mcp", "")).await
    else {
        panic!("mcp command replies");
    };
    assert!(text.contains("disconnected"));
}

struct HttpFixture {
    address: std::net::SocketAddr,
    closed: tokio::sync::mpsc::Receiver<()>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl HttpFixture {
    async fn start(
        oversized_method: &'static str,
        framing: &'static str,
        status: u16,
        media: &'static str,
    ) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (closed_tx, closed) = tokio::sync::mpsc::channel(8);
        let task = tokio::spawn(async move {
            // Dropping this set cancels request workers as well as the listener.
            let mut workers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    joined = workers.join_next(), if !workers.is_empty() => { joined.unwrap().unwrap(); }
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let closed_tx = closed_tx.clone();
                        workers.spawn(async move {
                            let mut stream = BufReader::new(stream);
                            let mut line = String::new();
                            stream.read_line(&mut line).await.unwrap();
                            let is_post = line.starts_with("POST ");
                            let mut length = 0;
                            loop {
                                line.clear();
                                stream.read_line(&mut line).await.unwrap();
                                if line == "\r\n" || line.is_empty() { break; }
                                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    length = value.trim().parse::<usize>().unwrap();
                                }
                            }
                            assert!(length < 65536);
                            let mut bytes = vec![0; length];
                            stream.read_exact(&mut bytes).await.unwrap();
                            let request: Value = if is_post { serde_json::from_slice(&bytes).unwrap() } else { Value::Null };
                            if !oversized_method.is_empty() && request["method"] == oversized_method {
                                let framing_header = match framing {
                                    "length" => "Content-Length: 1099511627776\r\n",
                                    "chunked" => "Transfer-Encoding: chunked\r\n",
                                    "close" => "",
                                    _ => panic!("unknown framing"),
                                };
                                let header = format!("HTTP/1.1 {status} Reply\r\nContent-Type: {media}\r\n{framing_header}Connection: close\r\n\r\n");
                                if stream.write_all(header.as_bytes()).await.is_err() { return; }
                                // Allocate one small chunk, never the advertised terabyte.
                                // Leave the body unfinished to distinguish a byte-limit
                                // rejection from EOF/parse failures or a request timeout.
                                if framing != "length" {
                                    let chunk = [b'x'; 4096];
                                    for _ in 0..=(http::MAX_HTTP_BYTES / chunk.len()) {
                                        let result = if framing == "chunked" {
                                            async {
                                                stream.write_all(b"1000\r\n").await?;
                                                stream.write_all(&chunk).await?;
                                                stream.write_all(b"\r\n").await
                                            }.await
                                        } else { stream.write_all(&chunk).await };
                                        if result.is_err() { break; }
                                    }
                                }
                                let _ = stream.flush().await;
                                let mut byte = [0];
                                let _ = stream.read(&mut byte).await;
                                let _ = closed_tx.send(()).await;
                                return;
                            }
                            let (status, body) = if !is_post {
                                ("405 Method Not Allowed", String::new())
                            } else if request.get("id").is_none() {
                                ("202 Accepted", String::new())
                            } else {
                                let result = match request["method"].as_str().unwrap() {
                                    "initialize" => json!({
                                        "protocolVersion": request["params"]["protocolVersion"],
                                        "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "loopback", "version": "1"}
                                    }),
                                    "tools/list" => json!({"tools": [{
                                        "name": "echo", "description": "Loopback echo",
                                        "inputSchema": {"type": "object", "properties": {}}
                                    }]}),
                                    "tools/call" => json!({
                                        "content": [{"type": "text", "text": "HTTP result"}],
                                        "structuredContent": request["params"]["arguments"],
                                        "isError": false
                                    }),
                                    _ => panic!("unexpected HTTP request: {request}"),
                                };
                                ("200 OK", json!({"jsonrpc": "2.0", "id": request["id"], "result": result}).to_string())
                            };
                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                            );
                            let _ = stream.write_all(response.as_bytes()).await;
                        });
                    }
                }
            }
        });
        Self {
            address,
            closed,
            task,
        }
    }

    async fn registry(&self) -> McpRegistry {
        let mut config = fixture("");
        config.command = None;
        config.url = Some(format!("http://{}/mcp", self.address));
        config.read_only = true;
        discover(
            BTreeMap::from([("local".into(), config)]),
            Vec::new(),
            Path::new("."),
        )
        .await
    }

    async fn assert_body_dropped(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), self.closed.recv())
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn streamable_http_discovers_and_calls_loopback_server() {
    let server = HttpFixture::start("", "", 200, "application/json").await;
    let registry = server.registry().await;
    assert_eq!(registry.servers()[0].status, McpStatus::Connected);
    assert!(registry.servers()[0].read_only);
    let result = tool(&registry, "echo")
        .execute(json!({"text": "structured"}), &ToolContext::new())
        .await
        .unwrap();
    let result: Value = serde_json::from_str(result.as_str().unwrap()).unwrap();
    assert_eq!(result["structuredContent"]["text"], "structured");
    close(&registry).await;
}

#[tokio::test]
async fn oversized_http_initialization_and_discovery_fail_before_timeout() {
    for (method, reason) in [
        ("initialize", "MCP initialization failed"),
        ("tools/list", "tool discovery failed"),
    ] {
        for framing in ["length", "chunked", "close"] {
            let mut server = HttpFixture::start(method, framing, 200, "application/json").await;
            let registry = server.registry().await;
            assert_eq!(
                registry.servers()[0].status,
                McpStatus::Failed(reason.into()),
                "{method}, {framing}"
            );
            assert!(registry.servers()[0].tools.is_empty());
            server.assert_body_dropped().await;
        }
    }
}

#[tokio::test]
async fn oversized_http_tool_success_error_and_sse_bodies_fail_before_timeout() {
    for (status, media) in [
        (200, "application/json"),
        (500, "application/json"),
        (400, "text/plain"),
        (200, "text/event-stream"),
    ] {
        for framing in ["length", "chunked", "close"] {
            let mut server = HttpFixture::start("tools/call", framing, status, media).await;
            let registry = server.registry().await;
            assert_eq!(registry.servers()[0].status, McpStatus::Connected);
            let error = tool(&registry, "echo")
                .execute(json!({}), &ToolContext::new())
                .await
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                ToolError::execution_error("MCP server request failed or disconnected").to_string(),
                "{status}, {media}, {framing}"
            );
            server.assert_body_dropped().await;
            close(&registry).await;
        }
    }
}

#[tokio::test]
async fn discovery_accepts_exact_tool_and_page_limits() {
    for (mode, count) in [("max-tools", 256), ("max-pages", 0)] {
        let registry = fixture_registry(mode).await;
        assert_eq!(registry.servers()[0].status, McpStatus::Connected, "{mode}");
        assert_eq!(registry.servers()[0].tools.len(), count, "{mode}");
        close(&registry).await;
    }
}

#[tokio::test]
async fn configured_timeouts_have_inclusive_bounds() {
    for timeout_secs in [0, 301, u64::MAX] {
        let mut config = fixture("");
        config.timeout_secs = timeout_secs;
        assert_eq!(
            connect(&config, Path::new(".")).await.err(),
            Some("timeoutSecs must be between 1 and 300")
        );
    }
    for timeout_secs in [1, 300] {
        let mut config = fixture("");
        config.timeout_secs = timeout_secs;
        let connection = connect(&config, Path::new(env!("CARGO_MANIFEST_DIR")))
            .await
            .unwrap();
        assert_eq!(connection.timeout, Duration::from_secs(timeout_secs));
        connection.close().await;
    }
}

#[test]
fn status_display_preserves_operator_messages_and_debug_is_informative() {
    for (status, text) in [
        (McpStatus::Connected, "connected"),
        (McpStatus::Disconnected, "disconnected"),
        (McpStatus::Disabled, "disabled"),
        (
            McpStatus::Untrusted,
            "not started: project server not trusted",
        ),
        (
            McpStatus::Failed("initialization timed out".into()),
            "initialization timed out",
        ),
    ] {
        assert_eq!(status.to_string(), text);
    }
    assert!(format!("{:?}", McpRegistry::default()).contains("servers"));
    assert!(
        format!(
            "{:?}",
            McpClientPlugin {
                registry: McpRegistry::default()
            }
        )
        .contains("McpClientPlugin")
    );
}

#[tokio::test]
async fn plugin_publishes_resource_and_tools_and_cleans_up() {
    let registry = fixture_registry("").await;
    let plugin = McpClientPlugin {
        registry: registry.clone(),
    };
    let mut server = Server::new();
    server.add_plugins(plugin).add_plugins(ToolsPlugin);
    server.finish().await.unwrap();
    assert_eq!(
        server.get_global::<McpRegistry>().unwrap().servers()[0].status,
        McpStatus::Connected
    );
    assert!(
        server
            .get_global::<ToolRegistry>()
            .unwrap()
            .definitions()
            .iter()
            .any(|tool| { tool.name == "mcp__local__echo" })
    );
    server.cleanup().await;
    assert_eq!(registry.servers()[0].status, McpStatus::Disconnected);
}

fn write_project(servers: Value) -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join(".mcp.json"),
        json!({ "mcpServers": servers }).to_string(),
    )
    .unwrap();
    project
}

/// A declaration whose launch leaves a file behind, to show it was started.
fn marker_server(marker: &Path) -> Value {
    json!({
        "command": "/bin/sh",
        "args": ["-c", format!("touch '{}'", marker.display())],
        "timeoutSecs": 1,
    })
}

fn trust_in(store: &Path, project: &Path) -> ProjectTrust {
    ProjectTrust::new(Some(store.join(crate::project_trust::STORE_FILE)), project)
}

async fn start(project: &Path, store: &Path, commands: &Arc<CommandRegistry>) -> McpClientPlugin {
    McpClientPlugin::start(project, None, commands.clone(), trust_in(store, project)).await
}

async fn mcp_reply(commands: &CommandRegistry, args: &str) -> Result<String, String> {
    match commands
        .dispatch(&CommandInvocation::new("mcp", args))
        .await
    {
        CommandOutcome::Reply(text) => Ok(text),
        CommandOutcome::Error(text) => Err(text),
        _ => panic!("mcp command is local"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn untrusted_project_servers_are_never_started() {
    let store = tempfile::tempdir().unwrap();
    let markers = tempfile::tempdir().unwrap();
    let marker = markers.path().join("started");
    let project = write_project(json!({ "hostile": marker_server(&marker) }));
    let commands = Arc::new(CommandRegistry::with_builtins());

    let plugin = start(project.path(), store.path(), &commands).await;

    assert!(!marker.exists(), "an untrusted project server was started");
    assert_eq!(plugin.registry.untrusted(), ["hostile"]);
    assert!(plugin.registry.entries[0].connection.is_none());
    assert!(plugin.registry.entries[0].tools.is_empty());
    // The person starting an unattended run is told what was held back and how
    // to decide; an interactive session shows the same line and `/mcp trust`.
    let notes = plugin.startup_notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("`hostile`") && notes[0].contains("/mcp trust"));
    let listing = mcp_reply(&commands, "").await.unwrap();
    assert!(listing.contains("`hostile`") && listing.contains("not trusted"));
    let review = mcp_reply(&commands, "trust").await.unwrap();
    assert!(review.contains("/bin/sh") && review.contains("/mcp trust hostile"));
    assert!(!marker.exists(), "reviewing a project server started it");
}

#[cfg(unix)]
#[tokio::test]
async fn trusting_a_declaration_starts_it_next_time_until_it_changes() {
    let store = tempfile::tempdir().unwrap();
    let markers = tempfile::tempdir().unwrap();
    let marker = markers.path().join("started");
    let project = write_project(json!({ "hostile": marker_server(&marker) }));
    let commands = Arc::new(CommandRegistry::with_builtins());

    let first = start(project.path(), store.path(), &commands).await;
    assert_eq!(first.registry.untrusted(), ["hostile"]);
    let reply = mcp_reply(&commands, "trust hostile").await.unwrap();
    assert!(reply.contains("next time Polaris starts"), "{reply}");
    assert!(!marker.exists(), "trusting a server must not start it");

    // A later start with the same declaration runs it.
    let commands = Arc::new(CommandRegistry::with_builtins());
    let second = start(project.path(), store.path(), &commands).await;
    assert!(marker.exists(), "a trusted project server did not start");
    assert!(second.registry.untrusted().is_empty());
    assert!(second.startup_notes().is_empty());

    // Any change to what would run asks again.
    std::fs::remove_file(&marker).unwrap();
    let elsewhere = markers.path().join("elsewhere");
    std::fs::write(
        project.path().join(".mcp.json"),
        json!({ "mcpServers": { "hostile": marker_server(&elsewhere) } }).to_string(),
    )
    .unwrap();
    let commands = Arc::new(CommandRegistry::with_builtins());
    let third = start(project.path(), store.path(), &commands).await;
    assert_eq!(third.registry.untrusted(), ["hostile"]);
    assert!(!elsewhere.exists() && !marker.exists());
}

#[test]
fn trust_for_one_project_does_not_cover_another() {
    let store = tempfile::tempdir().unwrap();
    let declared = json!({ "shared": { "command": "tool", "args": ["--serve"] } });
    let (first, second) = (write_project(declared.clone()), write_project(declared));
    let trust = trust_in(store.path(), first.path());
    config::load(first.path(), None, &trust);
    trust
        .grant_observed(&Subject::McpServer("shared".into()))
        .unwrap();

    let loaded = config::load(first.path(), None, &trust_in(store.path(), first.path()));
    assert!(loaded.servers.contains_key("shared") && loaded.withheld.is_empty());
    let loaded = config::load(second.path(), None, &trust_in(store.path(), second.path()));
    assert!(loaded.servers.is_empty());
    assert_eq!(loaded.withheld.len(), 1);
}

#[test]
fn a_project_cannot_mark_its_own_server_read_only() {
    let store = tempfile::tempdir().unwrap();
    let project = write_project(json!({ "claims": { "command": "tool", "readOnly": true } }));
    let user = tempfile::tempdir().unwrap();
    let user_path = user.path().join("mcp.json");
    std::fs::write(
        &user_path,
        r#"{"mcpServers":{"mine":{"command":"tool","readOnly":true}}}"#,
    )
    .unwrap();
    let trust = trust_in(store.path(), project.path());

    let loaded = config::load(project.path(), Some(&user_path), &trust);
    assert_eq!(loaded.withheld.len(), 1);
    assert!(loaded.errors.iter().any(|error| error.contains("readOnly")));
    // Only the user's own declaration can say a server is read-only.
    assert!(loaded.servers["mine"].read_only);

    trust
        .grant_observed(&Subject::McpServer("claims".into()))
        .unwrap();
    let loaded = config::load(project.path(), Some(&user_path), &trust);
    assert!(loaded.withheld.is_empty());
    assert!(!loaded.servers["claims"].read_only);
    assert!(loaded.servers["mine"].read_only);
}

#[test]
fn trust_commands_accept_only_servers_waiting_for_trust() {
    let waiting = McpRegistry::default().with_untrusted(["hostile".to_string()]);
    let summaries = [("hostile".to_string(), "runs \"tool\"".to_string())];
    let trust = ProjectTrust::default();

    let CommandOutcome::Error(text) = run_command("trust nosuch", &waiting, &summaries, &trust)
    else {
        panic!("a server that is not waiting is an error");
    };
    assert!(text.contains("not a project MCP server waiting for trust"));
    // Nothing was loaded and nothing can be stored, so nothing is trusted.
    let CommandOutcome::Error(text) = run_command("trust hostile", &waiting, &summaries, &trust)
    else {
        panic!("trust that cannot be saved is an error");
    };
    assert!(text.contains("Could not save trust"), "{text}");
    for args in ["bogus", "trust a b"] {
        let CommandOutcome::Error(text) = run_command(args, &waiting, &summaries, &trust) else {
            panic!("`{args}` is not a command");
        };
        assert!(text.contains("Usage: /mcp"), "{text}");
    }
    let CommandOutcome::Reply(text) = run_command("trust", &McpRegistry::default(), &[], &trust)
    else {
        panic!("nothing to review is not an error");
    };
    assert!(text.contains("No project MCP servers are waiting"));
}

#[test]
fn what_a_withheld_server_would_run_is_shown_safely() {
    let store = tempfile::tempdir().unwrap();
    let project = write_project(json!({
        "x": {
            "command": "run\u{1b}[2J",
            "args": ["a b"],
            "env": { "TOKEN": "hunter2" },
        },
        "far": { "url": "https://example.invalid/mcp" },
    }));
    let loaded = config::load(
        project.path(),
        None,
        &trust_in(store.path(), project.path()),
    );
    let summary = |name: &str| {
        loaded
            .withheld
            .iter()
            .find(|server| server.name == name)
            .unwrap()
            .summary()
    };
    assert!(!summary("x").contains('\u{1b}'), "{:?}", summary("x"));
    assert!(summary("x").contains("TOKEN") && !summary("x").contains("hunter2"));
    assert!(summary("far").contains("connects to"));
}

#[test]
fn only_a_bounded_number_of_untrusted_servers_are_listed() {
    let store = tempfile::tempdir().unwrap();
    let servers = (0..40)
        .map(|index| (format!("s{index}"), json!({ "command": "tool" })))
        .collect::<serde_json::Map<_, _>>();
    let project = write_project(Value::Object(servers));
    let loaded = config::load(
        project.path(),
        None,
        &trust_in(store.path(), project.path()),
    );
    assert_eq!(loaded.withheld.len(), 32);
    assert!(loaded.servers.is_empty());
    assert!(
        loaded
            .errors
            .iter()
            .any(|error| error.starts_with("8 more"))
    );
}

#[cfg(unix)]
#[tokio::test]
async fn disabled_and_invalid_project_declarations_never_start() {
    let store = tempfile::tempdir().unwrap();
    let markers = tempfile::tempdir().unwrap();
    let marker = markers.path().join("started");
    let mut disabled = marker_server(&marker);
    disabled["disabled"] = json!(true);
    let project = write_project(json!({
        "off": disabled,
        "bad__name": marker_server(&marker),
    }));
    let commands = Arc::new(CommandRegistry::with_builtins());

    let plugin = start(project.path(), store.path(), &commands).await;

    assert!(!marker.exists());
    // There is nothing to trust: neither declaration can run.
    assert!(plugin.registry.untrusted().is_empty());
    assert_eq!(plugin.registry.servers().len(), 2);
}

#[test]
fn an_oversized_project_declaration_file_is_refused() {
    let store = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let padding = " ".repeat(crate::project_trust::MAX_FILE_BYTES as usize + 1);
    std::fs::write(
        project.path().join(".mcp.json"),
        format!(r#"{{"mcpServers":{{}}}}{padding}"#),
    )
    .unwrap();
    let loaded = config::load(
        project.path(),
        None,
        &trust_in(store.path(), project.path()),
    );
    assert!(loaded.servers.is_empty());
    assert_eq!(loaded.errors.len(), 1);
    assert!(loaded.errors[0].contains("exceeds 1 MiB"));
}

#[cfg(unix)]
#[test]
fn a_pipe_is_not_read_as_a_project_declaration_file() {
    let store = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let fifo = project.path().join(".mcp.json");
    if !std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .is_ok_and(|status| status.success())
    {
        return;
    }
    // Reading a pipe with no writer would wait forever.
    let loaded = config::load(
        project.path(),
        None,
        &trust_in(store.path(), project.path()),
    );
    assert!(loaded.servers.is_empty());
    assert!(loaded.errors[0].contains("Not a regular file"));
}
