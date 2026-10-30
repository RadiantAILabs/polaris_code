//! `web_fetch` against a real HTTP server on the loopback interface.
//!
//! The tool refuses non-public addresses, so every test that needs a server
//! exempts `127.0.0.1` with [`WebToolsConfig::with_allowed_address`]. The tests
//! that check the refusal itself use the default configuration and count how
//! many requests reached the server: a refused fetch must reach it zero times.
//!
//! The permission tests go through the same gate `execute_tools` applies
//! (registry default, then the mode's policy), so they fail if the tool's
//! registered permission or a mode's overrides change.

use std::io::{BufRead, BufReader, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin};
use polaris_code::agents::code_agent::SecretRedactor;
use polaris_code::agents::{PermissionMode, PermissionPolicy, UnattendedGrants};
use polaris_code::tools::{
    ToolPolicyRegistry, WEB_FETCH, WebTools, WebToolsConfig, WebToolsPlugin,
};
use serde_json::{Value, json};

/// One request as the test server saw it.
struct Request {
    path: String,
    /// The request line and headers, as received.
    head: String,
}

/// What the test server answers.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    delay: Duration,
}

impl Reply {
    fn with(status: u16, content_type: Option<&str>, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: content_type
                .map(|value| ("Content-Type", value.to_owned()))
                .into_iter()
                .collect(),
            body: body.into(),
            delay: Duration::ZERO,
        }
    }

    fn ok(content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        Self::with(200, Some(content_type), body)
    }

    fn redirect(location: &str) -> Self {
        let mut reply = Self::with(302, None, Vec::new());
        reply.headers.push(("Location", location.to_owned()));
        reply
    }

    fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// A server on `127.0.0.1` that answers every request with `handler`'s reply
/// and closes the connection.
struct TestServer {
    addr: SocketAddr,
    hits: Arc<AtomicUsize>,
    shutdown: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(handler: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(handler);
        let counter = Arc::clone(&hits);
        let shutdown = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::clone(&shutdown);
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                if *stop.0.lock().unwrap() {
                    break;
                }
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let stop = Arc::clone(&stop);
                let handler = Arc::clone(&handler);
                let counter = Arc::clone(&counter);
                workers.push(thread::spawn(move || {
                    let mut reader = BufReader::new(stream);
                    let mut head = String::new();
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        head.push_str(&line);
                    }
                    counter.fetch_add(1, Ordering::SeqCst);
                    let path = head
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or_default()
                        .to_owned();
                    let reply = handler(&Request { path, head });
                    let (stopped, _) = stop
                        .1
                        .wait_timeout_while(stop.0.lock().unwrap(), reply.delay, |stopped| {
                            !*stopped
                        })
                        .unwrap();
                    if *stopped {
                        return;
                    }
                    drop(stopped);
                    let mut out = format!(
                        "HTTP/1.1 {} Reply\r\nContent-Length: {}\r\nConnection: close\r\n",
                        reply.status,
                        reply.body.len()
                    );
                    for (name, value) in &reply.headers {
                        out.push_str(&format!("{name}: {value}\r\n"));
                    }
                    out.push_str("\r\n");
                    let mut stream = reader.into_inner();
                    // The client may already have gone (size cap, timeout).
                    let _ = stream.write_all(out.as_bytes());
                    let _ = stream.write_all(&reply.body);
                    let _ = stream.flush();
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            addr,
            hits,
            shutdown,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        *self.shutdown.0.lock().unwrap() = true;
        self.shutdown.1.notify_all();
        // Wake the blocking accept, then join it and all request workers.
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

/// The configuration for talking to a [`TestServer`].
fn loopback() -> WebToolsConfig {
    WebToolsConfig::new().with_allowed_address(IpAddr::from([127, 0, 0, 1]))
}

fn registry(config: WebToolsConfig) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(WebTools::new(config));
    registry
}

async fn fetch(registry: &ToolRegistry, args: Value) -> Result<String, ToolError> {
    match registry.execute(WEB_FETCH, &args).await? {
        Value::String(text) => Ok(text),
        other => panic!("web_fetch returned a non-string result: {other}"),
    }
}

async fn get(config: WebToolsConfig, url: &str) -> Result<String, ToolError> {
    fetch(&registry(config), json!({ "url": url })).await
}

fn message(error: ToolError) -> String {
    error.to_string()
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_text_is_returned_with_where_it_came_from() {
    let server = TestServer::start(|_| Reply::ok("text/plain; charset=utf-8", "hello\nworld\n"));
    let url = server.url("/notes.txt");

    let out = get(loopback(), &url).await.unwrap();

    assert_eq!(
        out,
        format!("URL: {url}\nType: text/plain\n\nhello\nworld\n")
    );
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn html_comes_back_as_markdown_without_scripts() {
    let page = "<!doctype html><html><head><title>Guide</title>\
                <style>p { color: red }</style><script>var secret = 1;</script></head>\
                <body><nav>Home | About</nav><main><h1>Install</h1>\
                <p>Run <code>cargo add x</code> then read <a href=\"/docs/next\">the docs</a>.</p>\
                </main><footer>Copyright</footer></body></html>";
    let server = TestServer::start(move |_| Reply::ok("text/html; charset=utf-8", page));
    let url = server.url("/guide");

    let out = get(loopback(), &url).await.unwrap();

    assert!(
        out.contains("Type: text/html (converted to markdown)\n"),
        "{out}"
    );
    assert!(out.contains("Title: Guide\n"), "{out}");
    assert!(out.contains("# Install"), "{out}");
    assert!(out.contains("`cargo add x`"), "{out}");
    assert!(
        out.contains(&format!("[the docs](http://{}/docs/next)", server.addr)),
        "links are resolved against the page: {out}"
    );
    for dropped in ["var secret", "color: red", "Home | About", "Copyright"] {
        assert!(
            !out.contains(dropped),
            "{dropped:?} should be dropped: {out}"
        );
    }
}

#[tokio::test]
async fn a_credential_on_a_page_is_still_recognisable_after_conversion() {
    // Every tool result passes through `SecretRedactor` before the model sees
    // it. The conversion must leave credential shapes intact for it to find,
    // whether the key sits in a code block or in running text.
    let key = "sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm";
    let page = format!(
        "<html><body><pre>ANTHROPIC_API_KEY={key}</pre>\
         <p>Or paste <b>{key}</b> into the form.</p></body></html>"
    );
    let server = TestServer::start(move |_| Reply::ok("text/html", page.clone()));

    let out = get(loopback(), &server.url("/")).await.unwrap();

    assert!(
        out.contains(key),
        "the converter must not alter the key: {out}"
    );
    let scan = SecretRedactor::new(true).scan(&out);
    assert!(!scan.is_clean());
    assert!(!scan.text().contains(key), "{}", scan.text());
    assert!(scan.text().contains("[REDACTED:anthropic-api-key]"));
}

#[tokio::test]
async fn a_page_without_a_content_type_is_recognised_as_html() {
    let server =
        TestServer::start(|_| Reply::with(200, None, "<html><body><h2>Hi</h2></body></html>"));

    let out = get(loopback(), &server.url("/")).await.unwrap();

    assert!(
        out.contains("Type: no content type sent (converted to markdown)"),
        "{out}"
    );
    assert!(out.contains("## Hi"), "{out}");
}

#[tokio::test]
async fn the_declared_charset_is_honoured() {
    let server =
        TestServer::start(|_| Reply::ok("text/plain; charset=ISO-8859-1", b"caf\xe9".to_vec()));

    let out = get(loopback(), &server.url("/")).await.unwrap();

    assert!(out.ends_with("\n\ncafé"), "{out}");
}

#[tokio::test]
async fn the_request_identifies_itself_and_sends_no_credentials() {
    let seen = Arc::new(std::sync::Mutex::new(String::new()));
    let record = Arc::clone(&seen);
    let server = TestServer::start(move |request| {
        *record.lock().unwrap() = request.head.to_ascii_lowercase();
        Reply::ok("text/plain", "ok")
    });

    get(loopback(), &server.url("/")).await.unwrap();

    let head = seen.lock().unwrap().clone();
    assert!(head.starts_with("get / http/1.1"), "{head}");
    assert!(head.contains("user-agent: polaris-code/"), "{head}");
    assert!(head.contains("accept: text/html"), "{head}");
    for header in ["authorization", "cookie", "proxy-authorization"] {
        assert!(!head.contains(header), "{header} must not be sent: {head}");
    }
}

#[tokio::test]
async fn a_redirect_is_followed_and_reported() {
    let server = TestServer::start(|request| match request.path.as_str() {
        "/old" => Reply::redirect("/new"),
        _ => Reply::ok("text/plain", "moved here"),
    });

    let out = get(loopback(), &server.url("/old")).await.unwrap();

    assert!(
        out.starts_with(&format!("URL: {}\n", server.url("/new"))),
        "{out}"
    );
    assert!(
        out.contains(&format!("Redirected from: {}\n", server.url("/old"))),
        "{out}"
    );
    assert!(out.ends_with("\n\nmoved here"), "{out}");
    assert_eq!(server.hits(), 2);
}

#[tokio::test]
async fn too_many_redirects_end_the_fetch() {
    let server = TestServer::start(|_| Reply::redirect("/again"));
    let config = loopback().with_max_redirects(3);

    let error = get(config, &server.url("/again")).await.unwrap_err();

    let text = message(error);
    assert!(text.contains("more than 3 redirects"), "{text}");
    assert!(text.contains("/again"), "the last target is named: {text}");
    assert_eq!(server.hits(), 4, "the original request plus three followed");
}

#[tokio::test]
async fn zero_allowed_redirects_follow_none() {
    let server = TestServer::start(|_| Reply::redirect("/elsewhere"));

    let error = get(loopback().with_max_redirects(0), &server.url("/"))
        .await
        .unwrap_err();

    assert!(message(error).contains("more than 0 redirects"));
    assert_eq!(server.hits(), 1);
}

#[tokio::test]
async fn a_download_is_cut_at_the_byte_cap_and_says_so() {
    let server = TestServer::start(|_| Reply::ok("text/plain", "a".repeat(10_000)));
    let config = loopback().with_max_download_bytes(1000);

    let out = get(config, &server.url("/big")).await.unwrap();

    assert!(
        out.contains(
            "Note: the download stopped at 1000 of 10000 bytes, so the end of the page is missing."
        ),
        "{out}"
    );
    let body = out.split_once("\n\n").unwrap().1;
    assert_eq!(body.len(), 1000, "only what was downloaded is returned");
}

#[tokio::test]
async fn a_long_page_comes_back_in_pieces_that_join_to_the_whole() {
    let source: String = (0..200).map(|n| format!("line {n}: some text\n")).collect();
    let served = source.clone();
    let server = TestServer::start(move |_| Reply::ok("text/plain", served.clone()));
    let registry = registry(loopback().with_page_bytes(500));
    let url = server.url("/long");

    let mut joined = String::new();
    let mut offset = 0;
    let mut calls = 0;
    loop {
        calls += 1;
        assert!(calls < 100, "paging did not finish");
        let out = fetch(&registry, json!({ "url": url, "offset": offset }))
            .await
            .unwrap();
        let body = out.split_once("\n\n").unwrap().1;
        match body.split_once("\n… showed bytes ") {
            Some((piece, footer)) => {
                joined.push_str(piece);
                assert!(piece.len() <= 500, "a piece stays inside the page size");
                assert!(
                    footer.contains(&format!("of {} bytes of page text", source.len())),
                    "the footer gives the total: {footer}"
                );
                let next = footer.split("offset=").nth(1).expect("a next offset");
                assert!(
                    footer.contains(&format!("Next: web_fetch(url=\"{url}\"")),
                    "{footer}"
                );
                offset = next.trim_end_matches(')').parse().unwrap();
            }
            None => {
                joined.push_str(body);
                break;
            }
        }
    }

    assert!(calls > 1);
    assert_eq!(joined, source);
}

#[tokio::test]
async fn a_slow_server_ends_in_a_timeout_that_names_the_remedy() {
    let server =
        TestServer::start(|_| Reply::ok("text/plain", "late").after(Duration::from_secs(5)));
    let config = loopback().with_timeout(Duration::from_millis(500));

    let error = get(config, &server.url("/slow")).await.unwrap_err();

    let text = message(error);
    assert!(text.contains("no complete answer within"), "{text}");
    assert!(text.contains("try again"), "{text}");
}

#[tokio::test]
async fn an_error_status_names_the_status_and_what_to_do() {
    let server = TestServer::start(|_| Reply::with(404, Some("text/html"), "<h1>Not found</h1>"));

    let text = message(get(loopback(), &server.url("/missing")).await.unwrap_err());

    assert!(text.contains("404"), "{text}");
    assert!(text.contains("check that the URL is right"), "{text}");
    assert!(text.contains(&server.url("/missing")), "{text}");
}

#[tokio::test]
async fn redirect_locations_are_required_and_validated() {
    let missing = TestServer::start(|_| Reply::with(302, None, Vec::new()));
    let error = get(loopback(), &missing.url("/")).await.unwrap_err();
    assert!(matches!(error, ToolError::ExecutionError(_)));
    assert!(error.to_string().contains("302"));
    assert!(
        error
            .to_string()
            .contains("check that the URL is right and the page is public")
    );
    assert_eq!(missing.hits(), 1);

    let bad = TestServer::start(|_| Reply::redirect("http://[invalid"));
    let error = get(loopback(), &bad.url("/")).await.unwrap_err();
    assert!(matches!(error, ToolError::ExecutionError(_)));
    assert!(error.to_string().contains("which is not a valid URL"));
    assert_eq!(bad.hits(), 1);
}

#[tokio::test]
async fn error_statuses_explain_the_specific_remedy() {
    for (status, remedy) in [
        (401, "needs a login or refuses automated clients"),
        (403, "web_fetch sends no credentials"),
        (410, "check that the URL is right"),
        (429, "rate limiting; try again later"),
        (503, "server is failing; try again later"),
        (418, "check that the URL is right and the page is public"),
    ] {
        let server = TestServer::start(move |_| Reply::with(status, None, Vec::new()));
        let error = get(loopback(), &server.url("/")).await.unwrap_err();
        assert!(error.to_string().contains(&status.to_string()), "{error}");
        assert!(error.to_string().contains(remedy), "{error}");
    }
}

#[tokio::test]
async fn a_page_built_to_exhaust_the_converter_is_refused_with_a_remedy() {
    // 400,000 nested elements fit in the 2 MiB download cap. Parsed and walked
    // without a limit, such a page takes minutes and then overflows the stack.
    let page = format!("<html><body>{}", "<div>".repeat(400_000));
    let server = TestServer::start(move |_| Reply::ok("text/html", page.clone()));

    let text = message(get(loopback(), &server.url("/deep")).await.unwrap_err());

    assert!(text.contains(&server.url("/deep")), "{text}");
    assert!(text.contains("levels deep"), "{text}");
    assert!(text.contains("run_command"), "{text}");
}

#[tokio::test]
async fn a_binary_response_is_refused_by_name() {
    let server = TestServer::start(|_| Reply::ok("image/png", b"\x89PNG\r\n\x1a\n".to_vec()));

    let text = message(get(loopback(), &server.url("/logo.png")).await.unwrap_err());

    assert!(text.contains("image/png"), "{text}");
    assert!(text.contains("not text"), "{text}");
}

#[tokio::test]
async fn an_unlabelled_binary_body_is_refused_too() {
    let server = TestServer::start(|_| Reply::with(200, None, b"\0\0\0\x01binary".to_vec()));

    let text = message(get(loopback(), &server.url("/blob")).await.unwrap_err());

    assert!(text.contains("not text"), "{text}");
}

#[tokio::test]
async fn an_unreachable_host_is_an_error_not_a_hang() {
    // Hold the port for the test; no other process can bind it and answer.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let error = get(
        loopback().with_timeout(Duration::from_millis(500)),
        &format!("http://127.0.0.1:{port}/"),
    )
    .await
    .unwrap_err();

    assert!(matches!(error, ToolError::ExecutionError(_)), "{error}");
}

// ---------------------------------------------------------------------------
// What is refused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_default_configuration_never_reaches_loopback() {
    let server = TestServer::start(|_| Reply::ok("text/plain", "internal"));
    let port = server.addr.port();

    for (url, kind) in [
        (format!("http://127.0.0.1:{port}/"), "loopback"),
        (format!("http://localhost:{port}/"), "loopback"),
        (format!("http://2130706433:{port}/"), "loopback"),
        (format!("http://[::1]:{port}/"), "loopback"),
        (format!("http://[::ffff:127.0.0.1]:{port}/"), "loopback"),
        (format!("http://0.0.0.0:{port}/"), "unspecified"),
    ] {
        let error = get(WebToolsConfig::new(), &url).await.unwrap_err();
        assert!(
            matches!(error, ToolError::PermissionDenied(_)),
            "{url}: {error}"
        );
        assert!(error.to_string().contains(kind), "{url}: {error}");
    }
    assert_eq!(server.hits(), 0, "no request may reach a loopback server");
}

#[tokio::test]
async fn a_refused_address_is_named_with_its_kind_and_an_alternative() {
    let error = get(WebToolsConfig::new(), "http://127.0.0.1:9/")
        .await
        .unwrap_err();

    assert!(matches!(error, ToolError::PermissionDenied(_)), "{error}");
    let text = message(error);
    assert!(text.contains("127.0.0.1 is a loopback address"), "{text}");
    assert!(text.contains("public internet hosts only"), "{text}");
    assert!(text.contains("run_command"), "{text}");
}

#[tokio::test]
async fn private_and_link_local_addresses_are_refused_before_connecting() {
    for (url, kind) in [
        ("http://10.0.0.5/", "private-network"),
        ("http://192.168.1.1/admin", "private-network"),
        ("http://172.16.0.1/", "private-network"),
        ("http://169.254.169.254/latest/meta-data/", "link-local"),
        ("http://[fe80::1]/", "link-local"),
        ("http://[fd00::1]/", "private-network"),
        ("http://100.64.0.1/", "carrier-grade NAT"),
    ] {
        let error = get(WebToolsConfig::new(), url).await.unwrap_err();
        assert!(
            matches!(error, ToolError::PermissionDenied(_)),
            "{url}: {error}"
        );
        assert!(message(error).contains(kind), "{url}");
    }
}

#[tokio::test]
async fn a_redirect_to_a_private_address_is_refused_before_it_is_followed() {
    // The first hop is exempt (it is the test server); the second is the cloud
    // metadata address and is not.
    let server = TestServer::start(|_| Reply::redirect("http://169.254.169.254/latest/meta-data/"));

    let error = get(loopback(), &server.url("/start")).await.unwrap_err();

    assert!(matches!(error, ToolError::PermissionDenied(_)), "{error}");
    let text = message(error);
    assert!(
        text.contains("169.254.169.254 is a link-local address"),
        "{text}"
    );
    assert_eq!(server.hits(), 1, "only the first hop was requested");
}

#[tokio::test]
async fn a_redirect_to_another_scheme_is_refused() {
    let server = TestServer::start(|_| Reply::redirect("file:///etc/passwd"));

    let text = message(get(loopback(), &server.url("/")).await.unwrap_err());

    assert!(text.contains("only http:// and https://"), "{text}");
}

#[tokio::test]
async fn only_http_and_https_urls_without_credentials_are_accepted() {
    let registry = registry(WebToolsConfig::new());
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/x",
        "data:text/plain,hi",
    ] {
        let text = message(fetch(&registry, json!({ "url": url })).await.unwrap_err());
        assert!(text.contains("only http:// and https://"), "{url}: {text}");
    }

    let text = message(
        fetch(
            &registry,
            json!({ "url": "https://user:hunter2@example.com/" }),
        )
        .await
        .unwrap_err(),
    );
    assert!(text.contains("user name or password"), "{text}");
    assert!(
        !text.contains("hunter2"),
        "the password must not be echoed: {text}"
    );
}

#[tokio::test]
async fn a_malformed_or_missing_url_is_a_parameter_error() {
    let registry = registry(WebToolsConfig::new());

    let error = fetch(&registry, json!({ "url": "not a url" }))
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::ParameterError(_)), "{error}");
    assert!(message(error).contains("absolute http:// or https:// URL"));

    let error = fetch(&registry, json!({})).await.unwrap_err();
    assert!(matches!(error, ToolError::ParameterError(_)), "{error}");
    assert!(message(error).contains("url"));
}

// ---------------------------------------------------------------------------
// Permission
// ---------------------------------------------------------------------------

/// What `WebToolsPlugin` published for `web_fetch`: the permission it registers
/// the tool with, and the policy facts it declares, both read back from the
/// registries the plugin populated.
async fn published() -> (ToolPermission, ToolPolicyRegistry) {
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(WebToolsPlugin::default());
    server.finish().await.unwrap();
    let ctx = server.create_context();
    let registry = ctx.get_resource::<ToolRegistry>().unwrap();
    let registered = registry
        .permission(WEB_FETCH)
        .expect("the plugin registers web_fetch");
    let facts = server.get_global::<ToolPolicyRegistry>().unwrap().clone();
    (registered, facts)
}

/// The policy `execute_tools` resolves a call against: the mode's policy, with
/// the facts the capabilities declared.
fn policy_for(mode: &PermissionMode, facts: &ToolPolicyRegistry) -> PermissionPolicy {
    let mut policy = mode.permission_policy();
    policy.tools = facts.clone();
    policy
}

/// The gate `execute_tools` applies: the registry's permission, then the
/// mode's policy resolution.
fn gate(
    registered: ToolPermission,
    facts: &ToolPolicyRegistry,
    mode: &PermissionMode,
) -> ToolPermission {
    policy_for(mode, facts).resolve(WEB_FETCH, registered)
}

#[tokio::test]
async fn the_plugin_registers_web_fetch_as_confirm() {
    let mut server = Server::new();
    server
        .add_plugins(ToolsPlugin)
        .add_plugins(WebToolsPlugin::default());
    server.finish().await.unwrap();
    let ctx = server.create_context();
    let registry = ctx.get_resource::<ToolRegistry>().unwrap();

    assert!(registry.has(WEB_FETCH));
    assert_eq!(
        registry.permission(WEB_FETCH),
        Some(ToolPermission::Confirm)
    );
    let definition = registry
        .definitions()
        .into_iter()
        .find(|definition| definition.name == WEB_FETCH)
        .expect("web_fetch is exposed to the model");
    assert_eq!(definition.parameters["type"], "object");
    assert_eq!(definition.parameters["properties"]["url"]["type"], "string");
    assert!(definition.parameters["properties"].get("offset").is_some());
    assert_eq!(definition.parameters["required"], json!(["url"]));
    assert!(
        definition.description.contains("public"),
        "the model is told what it can reach: {}",
        definition.description
    );
}

#[tokio::test]
async fn every_attended_mode_asks_before_the_network_is_used() {
    let (registered, facts) = published().await;

    for mode in [
        PermissionMode::Confirm,
        PermissionMode::AutoEdit,
        PermissionMode::Auto,
        PermissionMode::Plan,
    ] {
        assert_eq!(
            gate(registered, &facts, &mode),
            ToolPermission::Confirm,
            "{} must ask",
            mode.label()
        );
        // An override that allows the tool does not take the prompt away.
        let mut policy = policy_for(&mode, &facts);
        policy
            .overrides
            .insert(WEB_FETCH.to_owned(), ToolPermission::Allow);
        assert_eq!(
            policy.resolve(WEB_FETCH, registered),
            ToolPermission::Confirm,
            "{} must still ask after an allow override",
            mode.label()
        );
    }
}

#[tokio::test]
async fn an_unattended_run_refuses_web_fetch_unless_it_grants_it() {
    let (registered, facts) = published().await;
    let mode = PermissionMode::Unattended(UnattendedGrants::default());
    assert_eq!(gate(registered, &facts, &mode), ToolPermission::Deny);

    // Granting the tool by name is an explicit `Allow` override on the policy.
    let mut policy = policy_for(&mode, &facts);
    policy
        .overrides
        .insert(WEB_FETCH.to_owned(), ToolPermission::Allow);
    assert_eq!(policy.resolve(WEB_FETCH, registered), ToolPermission::Allow);
    // The grant is for this tool only.
    assert_eq!(
        policy.resolve("run_command", ToolPermission::Confirm),
        ToolPermission::Deny
    );
}
