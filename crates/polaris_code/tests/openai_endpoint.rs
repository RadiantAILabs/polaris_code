//! `--provider openai` reaches an OpenAI-compatible server through
//! `OPENAI_BASE_URL`.
//!
//! `--provider openai` reaches an OpenAI-compatible server through
//! `OPENAI_BASE_URL`.
//!
//! `OpenAiPlugin` has no endpoint setter of its own: the client underneath it
//! reads the variable when the provider is built. `providers` relies on that to
//! support Ollama, LM Studio, vLLM and gateways, so it is pinned here against a
//! loopback server — a dependency update that stopped reading the variable
//! would otherwise leave `--provider openai` quietly talking to api.openai.com.
//!
//! Two registration paths are pinned. `OpenAiPlugin` is what `install` still
//! uses. The binary itself registers through `ReadyProvider::install_with_options`,
//! which builds a raw `OpenAiProvider` inside `ModelRunPlugin`, so the same
//! check runs through `providers::prepare` and that method.
//!
//! This file holds exactly one test: it sets process-wide variables, and no
//! other test in the process may read the environment while it does. Both
//! paths share that one test and one listener for the same reason.

use std::num::NonZeroU32;

use polaris::models::llm::{LlmRequest, Message};
use polaris::models::pricing::PriceStore;
use polaris::models::{ModelRegistry, ModelsPlugin, OpenAiPlugin};
use polaris::system::server::Server;
use polaris_code::credentials::CredentialStore;
use polaris_code::model_run::ModelRunOptions;
use polaris_code::providers::{self, Provider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const KEY_VAR: &str = "OPENAI_ENDPOINT_TEST_KEY";
/// The variable `--provider openai` reads its key from.
const BINARY_KEY_VAR: &str = "OPENAI_API_KEY";

/// What one request looked like on the wire.
struct Seen {
    request_line: String,
    authorization: String,
    body: serde_json::Value,
}

/// Accept one connection, record its request line and credentials, and answer
/// with an error the client reports without retrying.
async fn serve_once(listener: &TcpListener) -> Seen {
    let (mut socket, _) = listener.accept().await.expect("a request arrives");
    let mut request = Vec::new();
    let head_end = loop {
        let mut buffer = [0u8; 4096];
        let read = socket.read(&mut buffer).await.expect("the request is read");
        assert_ne!(read, 0, "connection ended before the headers");
        request.extend_from_slice(&buffer[..read]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
        assert!(
            request.len() <= 16 * 1024,
            "headers exceeded the test limit"
        );
    };
    let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
    let body_len = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(body_len <= 64 * 1024, "body exceeded the test limit");
    // Drain the body before closing so unread segments cannot reset the socket.
    while request.len() < head_end + body_len {
        let mut buffer = [0u8; 4096];
        let read = socket.read(&mut buffer).await.expect("the body is read");
        assert_ne!(read, 0, "connection ended before the body");
        request.extend_from_slice(&buffer[..read]);
    }
    let header = |name: &str| {
        head.lines()
            .find(|line| line.to_ascii_lowercase().starts_with(name))
            .unwrap_or_default()
            .to_string()
    };
    let body = r#"{"error":{"message":"loopback","type":"invalid_request_error"}}"#;
    let reply = format!(
        "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
         content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(reply.as_bytes())
        .await
        .expect("the reply is written");
    Seen {
        request_line: head.lines().next().unwrap_or_default().to_string(),
        authorization: header("authorization"),
        body: serde_json::from_slice(&request[head_end..head_end + body_len])
            .expect("the request body is JSON"),
    }
}

/// Send one request through `registry`'s `openai` provider and return what the
/// loopback server saw. The server answers 401, so the call must fail with the
/// server's own message.
async fn send_through(registry: &ModelRegistry, listener: &TcpListener, label: &str) -> Seen {
    let llm = registry
        .llm("openai/local-model")
        .expect("any model name resolves; the server decides whether it exists");
    let request = LlmRequest {
        messages: vec![Message::user("hello")],
        ..Default::default()
    };
    let (result, seen) = tokio::join!(llm.generate(request), serve_once(listener));
    assert!(
        seen.request_line.starts_with("POST /v1/responses "),
        "{label}: the Responses API path is appended to the base url: {}",
        seen.request_line
    );
    let error = result.expect_err("the loopback server refuses the request");
    assert!(error.to_string().contains("loopback"), "{label}: {error}");
    seen
}

#[tokio::test]
async fn the_openai_provider_sends_its_requests_to_openai_base_url() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a loopback port");
    let address = listener.local_addr().expect("the bound address");
    let endpoint = format!("http://{address}/v1");
    // SAFETY: this is the only test in the process, so no other thread reads
    // or writes the environment while it is changed.
    unsafe {
        std::env::set_var("OPENAI_BASE_URL", &endpoint);
        std::env::set_var(KEY_VAR, "loopback-key");
        std::env::set_var(BINARY_KEY_VAR, "loopback-binary-key");
    }

    // The plugin path.
    let mut server = Server::new();
    server.add_plugins(ModelsPlugin);
    server.add_plugins(OpenAiPlugin::from_env(KEY_VAR));
    server.finish().await.expect("the server builds");
    let registry = server
        .get_global::<ModelRegistry>()
        .expect("the model registry");
    assert_eq!(
        registry
            .llm_provider("openai")
            .expect("the openai provider is registered")
            .endpoint()
            .as_deref(),
        Some(endpoint.as_str()),
        "the provider reports the endpoint it was pointed at"
    );
    let seen = send_through(&registry, &listener, "OpenAiPlugin").await;
    assert_eq!(seen.authorization, "authorization: Bearer loopback-key");

    // The path the binary takes: the startup check, then `install_with_options`
    // with a raw `OpenAiProvider` inside `ModelRunPlugin`, as `main.rs` calls it.
    let ready = providers::prepare(
        Provider::OpenAi,
        None,
        &providers::process_env,
        &None::<CredentialStore>,
    )
    .await
    .expect("the startup check accepts the key and the base url");
    assert_eq!(
        ready.report().endpoint(),
        Some(endpoint.as_str()),
        "the startup report names the endpoint"
    );
    let mut server = Server::new();
    server.add_plugins(ModelsPlugin);
    ready.install_with_options(
        &mut server,
        NonZeroU32::new(1024).unwrap(),
        PriceStore::builtin(),
        ModelRunOptions {
            max_output_tokens: NonZeroU32::new(1024),
            ..ModelRunOptions::default()
        },
    );
    server.finish().await.expect("the server builds");
    let registry = server
        .get_global::<ModelRegistry>()
        .expect("the model registry");
    assert_eq!(
        registry
            .llm_provider("openai")
            .expect("the openai provider is registered")
            .endpoint()
            .as_deref(),
        Some(endpoint.as_str()),
        "the decorated provider reports the endpoint it was pointed at"
    );
    let seen = send_through(&registry, &listener, "install_with_options").await;
    assert_eq!(
        seen.authorization,
        "authorization: Bearer loopback-binary-key"
    );
    assert_eq!(
        seen.body["max_output_tokens"], 1024,
        "the run-wide output cap reaches the wire: {}",
        seen.body
    );
}
