//! Proof that a failing model provider is retried by the kind of failure,
//! cannot hang a turn, and is reported once, in one typed message.
//!
//! Each test drives one real turn through the real agent graph with a scripted
//! provider standing in for the network — no key, no connection — and reads back
//! what the provider was asked, how long the agent waited between asks, what
//! the front end was shown, and what the headless `json` result reports.
//!
//! The provider registers itself as `anthropic` so the messages carry the
//! wording a real Anthropic failure would, and the retry policy is shrunk to
//! milliseconds through the same `PolicySnapshot` resource the agent reads.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use polaris::graph::hooks::HooksAPI;
use polaris::graph::hooks::events::GraphEvent;
use polaris::graph::hooks::schedule::OnSystemStart;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{
    IO_TYPE, IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin,
};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::policy::{PolicySnapshot, RetryPolicy};
use polaris_code::agents::trajectory::SESSION_GRAPH_IRI;
use polaris_code::agents::{
    AgentConfig, CodeAgent, CodeAgentPlugin, FailureKind, LoopState, TurnFailure,
};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider, OutputFormat, RunReport};
use polaris_code::run::{self, TurnOutcome, TurnReason};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::{OxigraphPlugin, OxigraphStore};
use tokio::sync::Notify;

const ANSWER: &str = "all good.";
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// What one streaming call of the scripted provider does.
enum Step {
    /// The call is refused before any event.
    Fail(GenerationError),
    /// The call streams a short answer and stops cleanly.
    Answer,
    /// The call opens and then never produces an event.
    Stall,
    /// The call streams some text and then fails, so the text has been shown.
    TextThenFail(GenerationError),
}

/// A provider that follows a script and records how it was used.
struct Scripted {
    script: Box<dyn Fn(usize) -> Step + Send + Sync>,
    stream_calls: AtomicUsize,
    stream_times: Mutex<Vec<Instant>>,
    /// How many non-streaming calls were made. Only error recovery makes one.
    generate_calls: AtomicUsize,
}

impl Scripted {
    fn new(script: impl Fn(usize) -> Step + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            script: Box::new(script),
            stream_calls: AtomicUsize::new(0),
            stream_times: Mutex::new(Vec::new()),
            generate_calls: AtomicUsize::new(0),
        })
    }

    fn streams(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }

    fn recovery_requests(&self) -> usize {
        self.generate_calls.load(Ordering::SeqCst)
    }
}

fn text_events(text: &str) -> Vec<Result<StreamEvent, GenerationError>> {
    vec![
        Ok(StreamEvent::ContentBlockStart {
            index: 0,
            block: ContentBlockStartData::Text,
        }),
        Ok(StreamEvent::ContentBlockDelta {
            index: 0,
            delta: ContentBlockDelta::Text(text.to_string()),
        }),
    ]
}

/// The handle the registry owns, sharing the scripted state with the test.
struct Handle(Arc<Scripted>);

impl LlmProvider for Handle {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.0.generate_calls.fetch_add(1, Ordering::SeqCst);
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("recovered")],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        let call = self.0.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.0.stream_times.lock().unwrap().push(Instant::now());
        match (self.0.script)(call) {
            Step::Fail(error) => Err(error),
            Step::Stall => Ok(futures_util::stream::pending().boxed()),
            Step::TextThenFail(error) => {
                let mut events = text_events("partial ");
                events.push(Err(error));
                Ok(futures_util::stream::iter(events).boxed())
            }
            Step::Answer => {
                let mut events = text_events(ANSWER);
                events.push(Ok(StreamEvent::ContentBlockStop { index: 0 }));
                events.push(Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage {
                        input_tokens: Some(10),
                        output_tokens: Some(5),
                        total_tokens: Some(15),
                        ..Default::default()
                    },
                    id: None,
                    model: None,
                }));
                Ok(futures_util::stream::iter(events).boxed())
            }
        }
    }
}

struct ScriptedModelPlugin(Arc<Scripted>);

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_anthropic";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the scripted provider")
            .register_llm_provider(Handle(self.0.clone()));
    }
}

/// A retry policy fast enough for a test: the same shape as the default, with
/// waits in milliseconds.
fn fast(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(5),
        max_delay: Duration::from_millis(500),
        attempt_timeout: Duration::from_secs(10),
    }
}

/// What one driven turn left behind.
struct Run {
    outcome: TurnOutcome,
    /// Every message the front end was shown, in order.
    shown: Vec<IOMessage>,
    /// The failure the turn recorded on the session's `LoopState`.
    recorded: Option<TurnFailure>,
    report: RunReport,
    /// The name of every graph system that started, in order.
    nodes: Vec<String>,
    /// Every `retry` decision the turn wrote to the session graph, in order.
    retry_decisions: Vec<RetryDecision>,
}

/// One `retry` decision as recorded on the trajectory.
struct RetryDecision {
    chosen: String,
    consulted: Vec<String>,
}

impl RetryDecision {
    fn consulted(&self, entry: &str) -> bool {
        self.consulted.iter().any(|policy| policy == entry)
    }
}

impl Run {
    /// The text of every message tagged as an error.
    fn error_rows(&self) -> Vec<String> {
        self.shown
            .iter()
            .filter(|message| message.metadata.get(IO_TYPE).map(String::as_str) == Some("error"))
            .map(text_of)
            .collect()
    }

    /// Everything the front end was shown, as one searchable string.
    fn everything_shown(&self) -> String {
        self.shown
            .iter()
            .map(text_of)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The headless `json` object.
    fn json(&self) -> serde_json::Value {
        let payload = self
            .report
            .stdout_payload(OutputFormat::Json)
            .expect("`json` always prints one object");
        serde_json::from_str(&payload).expect("the object parses")
    }

    fn failure(&self) -> &TurnFailure {
        match &self.outcome.reason {
            TurnReason::Failed(failure) => failure,
            other => panic!("the turn should have failed, not {other:?}"),
        }
    }

    /// How many times the graph started the system called `name`.
    fn started(&self, name: &str) -> usize {
        self.nodes.iter().filter(|node| *node == name).count()
    }

    /// What each retry decision chose, in order.
    fn chosen(&self) -> Vec<&str> {
        self.retry_decisions
            .iter()
            .map(|decision| decision.chosen.as_str())
            .collect()
    }
}

/// Every `retry` decision in the session graph, ordered by when it was written.
///
/// The decision's IRI ends in a process-monotonic counter, so sorting on it
/// recovers the order whatever order the store returns its solutions in.
fn retry_decisions(store: &OxigraphStore) -> Vec<RetryDecision> {
    use oxigraph::sparql::QueryResults;

    let query = format!(
        "SELECT ?d ?chose ?policy WHERE {{ GRAPH <{SESSION_GRAPH_IRI}> {{ \
         ?d a <urn:polaris:session:Decision> ; \
            <urn:polaris:session:kind> \"retry\" ; \
            <urn:polaris:session:chose> ?chose ; \
            <urn:polaris:session:consultedPolicy> ?policy }} }}"
    );
    let QueryResults::Solutions(solutions) = store.query(&query).unwrap() else {
        panic!("SELECT returns solutions");
    };
    let literal = |solution: &oxigraph::sparql::QuerySolution, name: &str| {
        solution
            .get(name)
            .map(ToString::to_string)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string()
    };
    let mut by_decision: std::collections::BTreeMap<u64, RetryDecision> = Default::default();
    for solution in solutions.flatten() {
        let iri = solution
            .get("d")
            .map(ToString::to_string)
            .unwrap_or_default();
        let sequence = iri
            .trim_end_matches('>')
            .rsplit('/')
            .next()
            .and_then(|tail| tail.parse().ok())
            .unwrap_or(0);
        let decision = by_decision
            .entry(sequence)
            .or_insert_with(|| RetryDecision {
                chosen: literal(&solution, "chose"),
                consulted: Vec::new(),
            });
        decision.consulted.push(literal(&solution, "policy"));
    }
    by_decision.into_values().collect()
}

/// What a driven turn runs under.
#[derive(Clone, Copy)]
struct Setup {
    /// The executor's default loop limit, as `--max-turns` sets it.
    max_turns: usize,
    /// Interrupt the turn the moment it starts waiting out a backoff.
    interrupt_in_backoff: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            max_turns: 25,
            interrupt_in_backoff: false,
        }
    }
}

fn text_of(message: &IOMessage) -> String {
    match &message.content {
        IOContent::Text(text) => text.clone(),
        _ => String::new(),
    }
}

/// Drive one turn of the real coding agent against `provider`, under `retry`.
async fn drive(provider: &Arc<Scripted>, retry: RetryPolicy) -> Run {
    drive_with(provider, retry, Setup::default()).await
}

/// Like [`drive`], under `setup`. When it asks for an interrupt, the turn is
/// asked to stop, the way Ctrl-C does, as soon as the graph starts the backoff
/// system: by then the provider has been called and the turn is registered, so
/// the request cannot arrive too early to be seen.
async fn drive_with(provider: &Arc<Scripted>, retry: RetryPolicy, setup: Setup) -> Run {
    let workspace = tempfile::tempdir().unwrap();
    let data_dir = tempfile::tempdir().unwrap();
    let (front_end, input_tx, mut output_rx) = ExecIOProvider::new(256);
    let front_end = Arc::new(front_end);

    let nodes = Arc::new(Mutex::new(Vec::<String>::new()));
    let backoff_started = Arc::new(Notify::new());
    let mut server = Server::new();
    server.insert_api(HooksAPI::new());
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin(provider.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.path().to_str().unwrap(),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(front_end.clone()));
    server.finish().await.unwrap();
    {
        let nodes = nodes.clone();
        let backoff_started = backoff_started.clone();
        server
            .api::<HooksAPI>()
            .unwrap()
            .register_observer::<OnSystemStart, _>(
                "record_started_systems",
                move |event: &GraphEvent| {
                    if let GraphEvent::SystemStart { node_name, .. } = event {
                        nodes.lock().unwrap().push(node_name.to_string());
                        if node_name.as_str() == "wait_to_retry" {
                            backoff_started.notify_one();
                        }
                    }
                },
            )
            .unwrap();
    }

    let persistence = server.api::<PersistenceAPI>().unwrap();
    let sessions = server.api::<SessionsAPI>().unwrap();
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).unwrap();

    let session_id = SessionId::new();
    let config = AgentConfig::new("anthropic/test-model", workspace.path().to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            polaris::graph::GraphExecutor::new().with_default_max_iterations(setup.max_turns),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .unwrap();
    sessions
        .with_context(&session_id, move |ctx| {
            ctx.get_resource_mut::<PolicySnapshot>().unwrap().retry = retry;
        })
        .await
        .unwrap();

    input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("say hi".to_string()),
        ))
        .await
        .unwrap();
    drop(input_tx);
    let drain = tokio::spawn(async move {
        let mut shown = Vec::new();
        while let Some(message) = output_rx.recv().await {
            shown.push(message);
        }
        shown
    });

    let interrupt = async {
        if setup.interrupt_in_backoff {
            backoff_started.notified().await;
            assert!(run::request_interrupt(sessions, &session_id));
        }
    };
    let (outcome, ()) = tokio::join!(run::drive_turn(sessions, &session_id), interrupt);
    let recorded = sessions
        .with_context(&session_id, |ctx| {
            ctx.get_resource::<LoopState>()
                .ok()
                .and_then(|state| state.failure.clone())
        })
        .await
        .unwrap_or_default();
    front_end.close().await;
    let shown = drain.await.unwrap();
    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    let store = OxigraphStore::clone(&server.get_global::<OxigraphStore>().unwrap());
    let nodes = nodes.lock().unwrap().clone();
    Run {
        outcome,
        shown,
        recorded,
        report,
        nodes,
        retry_decisions: retry_decisions(&store),
    }
}

fn http(status: u16, error_type: &str, message: &str) -> GenerationError {
    GenerationError::Provider {
        status: Some(status),
        message: serde_json::json!({
            "type": "error",
            "error": {"type": error_type, "message": message},
            "request_id": "req_011CZ",
        })
        .to_string(),
        source: None,
    }
}

/// What a front end must never be shown for a failed model call: the old
/// three-fold report and the provider's raw body.
fn assert_nothing_stale_was_shown(run: &Run) {
    let shown = run.everything_shown();
    for stale in [
        "[System Error]",
        "LLM error",
        "I encountered an error processing your request",
        "req_011CZ",
        "node_",
    ] {
        assert!(!shown.contains(stale), "{stale:?} was shown:\n{shown}");
    }
}

#[tokio::test]
async fn an_authentication_failure_is_not_retried_and_says_what_to_fix() {
    let provider =
        Scripted::new(|_| Step::Fail(http(401, "authentication_error", "invalid x-api-key")));
    let run = drive(&provider, fast(5)).await;

    assert_eq!(
        provider.streams(),
        1,
        "a rejected key is not worth a second try"
    );
    assert_eq!(
        provider.recovery_requests(),
        0,
        "and is not worth a recovery request either"
    );
    let failure = run.failure();
    let expected = failure.message.as_str();
    // The hint's exact wording belongs to the classifier's own tests. This turn
    // has to show the provider's rejection and name the variable to check, and
    // every surface below has to carry that same text.
    assert!(
        expected.starts_with("Anthropic rejected the API key (HTTP 401, authentication_error). "),
        "{expected}"
    );
    assert!(expected.contains("ANTHROPIC_API_KEY"), "{expected}");
    assert_eq!(failure.kind, FailureKind::Auth);
    assert_eq!(run.outcome.reason.exit_code(), 1);
    assert_eq!(run.recorded.as_ref(), Some(failure));

    assert_eq!(run.error_rows(), [expected], "one message, shown once");
    assert_nothing_stale_was_shown(&run);

    let json = run.json();
    assert_eq!(json["status"], "failed");
    assert_eq!(json["error"], expected);
    assert_eq!(json["error_kind"], "auth");
    assert!(json["result"].is_null());
}

#[tokio::test]
async fn a_rate_limit_waits_as_long_as_the_provider_asked() {
    let provider = Scripted::new(|call| match call {
        0 => Step::Fail(GenerationError::RateLimited {
            retry_after: Some(Duration::from_millis(250)),
        }),
        _ => Step::Answer,
    });
    // The computed backoff would be about 5 ms, so a gap of 250 ms can only
    // come from the provider's own request.
    let run = drive(&provider, fast(5)).await;

    assert_eq!(run.outcome.reason, TurnReason::Completed);
    assert_eq!(run.outcome.final_text.as_deref(), Some(ANSWER));
    assert_eq!(provider.streams(), 2);
    let times = provider.stream_times.lock().unwrap();
    let waited = times[1].duration_since(times[0]);
    assert!(
        waited >= Duration::from_millis(250),
        "the second request came after {waited:?}"
    );
    assert!(
        run.error_rows().is_empty(),
        "a retried failure is not an error row"
    );
}

#[tokio::test]
async fn a_rate_limit_that_asks_for_longer_than_the_cap_is_not_waited_out() {
    let provider = Scripted::new(|_| {
        Step::Fail(GenerationError::RateLimited {
            retry_after: Some(Duration::from_secs(3600)),
        })
    });
    let run = drive(&provider, fast(5)).await;

    assert_eq!(provider.streams(), 1);
    let failure = run.failure();
    assert_eq!(failure.kind, FailureKind::RateLimit);
    assert_eq!(
        failure.message,
        "Anthropic rate limited the request and asked for a wait of 3600 s."
    );
    assert_eq!(run.json()["error_kind"], "rate_limit");
}

#[tokio::test]
async fn an_overloaded_provider_is_retried_then_reported_once_with_its_typed_message() {
    let provider = Scripted::new(|_| Step::Fail(http(529, "overloaded_error", "Overloaded")));
    let run = drive(&provider, fast(3)).await;

    assert_eq!(provider.streams(), 3, "the whole attempt budget was used");
    assert_eq!(provider.recovery_requests(), 0);
    let failure = run.failure();
    assert_eq!(failure.kind, FailureKind::Overloaded);
    assert!(
        failure.message.starts_with(
            "Anthropic is overloaded or unavailable (HTTP 529, overloaded_error). \
             Gave up after 2 retries over "
        ),
        "{}",
        failure.message
    );
    assert!(failure.message.ends_with(" s."), "{}", failure.message);

    assert_eq!(run.error_rows(), std::slice::from_ref(&failure.message));
    assert_nothing_stale_was_shown(&run);
    let json = run.json();
    assert_eq!(json["error"], failure.message);
    assert_eq!(json["error_kind"], "overloaded");
}

#[tokio::test]
async fn a_malformed_request_is_not_retried() {
    let provider = Scripted::new(|_| {
        Step::Fail(http(
            400,
            "invalid_request_error",
            "prompt is too long: 250000 tokens > 200000 maximum",
        ))
    });
    let run = drive(&provider, fast(5)).await;

    assert_eq!(provider.streams(), 1);
    let failure = run.failure();
    assert_eq!(failure.kind, FailureKind::InvalidRequest);
    assert_eq!(
        failure.message,
        "Anthropic rejected the request (HTTP 400, invalid_request_error): \
         prompt is too long: 250000 tokens > 200000 maximum"
    );
    assert_eq!(run.json()["error_kind"], "invalid_request");
}

#[tokio::test]
async fn a_provider_that_goes_quiet_times_out_and_is_retried() {
    let provider = Scripted::new(|_| Step::Stall);
    let retry = RetryPolicy {
        attempt_timeout: Duration::from_millis(80),
        ..fast(2)
    };
    let started = Instant::now();
    let run = drive(&provider, retry).await;

    assert!(
        started.elapsed() >= Duration::from_millis(160),
        "two attempts each waited out their timeout"
    );
    assert_eq!(provider.streams(), 2);
    let failure = run.failure();
    assert_eq!(failure.kind, FailureKind::Timeout);
    assert!(
        failure
            .message
            .starts_with("No response from Anthropic for ")
            && failure.message.contains("Gave up after 1 retry over "),
        "{}",
        failure.message
    );
    assert_eq!(run.json()["error_kind"], "timeout");
}

#[tokio::test]
async fn a_failure_after_text_was_shown_is_not_run_again() {
    let provider =
        Scripted::new(|_| Step::TextThenFail(GenerationError::Http("connection reset".into())));
    let run = drive(&provider, fast(5)).await;

    assert_eq!(provider.streams(), 1, "shown text cannot be taken back");
    let failure = run.failure();
    assert_eq!(failure.kind, FailureKind::Network);
    assert!(
        failure
            .message
            .ends_with("The response had already started, so it was not retried."),
        "{}",
        failure.message
    );
    assert!(run.everything_shown().contains("partial"));
}

#[tokio::test]
async fn a_connection_failure_is_retried_until_the_budget_is_spent() {
    let provider = Scripted::new(|_| {
        Step::Fail(GenerationError::Http(
            "error sending request for url (https://api.anthropic.com/v1/messages)".into(),
        ))
    });
    let run = drive(&provider, fast(4)).await;

    assert_eq!(provider.streams(), 4);
    assert_eq!(run.failure().kind, FailureKind::Network);
    assert_eq!(run.json()["error_kind"], "network");
}

#[tokio::test]
async fn an_interrupt_during_a_backoff_ends_the_turn_at_once() {
    // The provider asks for a five-second wait. Waiting it out would take the
    // test past its bound; an interrupt has to cut the wait short.
    let provider = Scripted::new(|_| {
        Step::Fail(GenerationError::RateLimited {
            retry_after: Some(Duration::from_secs(5)),
        })
    });
    let retry = RetryPolicy {
        max_delay: Duration::from_secs(10),
        ..fast(5)
    };
    let started = Instant::now();
    let setup = Setup {
        interrupt_in_backoff: true,
        ..Setup::default()
    };
    let run = drive_with(&provider, retry, setup).await;

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the interrupt did not cut the wait short: {:?}",
        started.elapsed()
    );
    assert_eq!(run.outcome.reason, TurnReason::Interrupted);
    assert_eq!(provider.streams(), 1, "no second attempt was started");
    assert!(run.error_rows().is_empty(), "an interrupt is not a failure");
}

#[tokio::test]
async fn a_transient_failure_is_retried_by_the_graphs_retry_stage() {
    let provider = Scripted::new(|call| match call {
        0 | 1 => Step::Fail(http(529, "overloaded_error", "Overloaded")),
        _ => Step::Answer,
    });
    let run = drive(&provider, fast(5)).await;

    assert_eq!(run.outcome.reason, TurnReason::Completed);
    assert_eq!(provider.streams(), 3);
    // One attempt per `act`, one decision and one backoff per failure, and the
    // response accepted once. Nothing was given up on or reported.
    assert_eq!(run.started("act"), 3);
    assert_eq!(run.started("decide_retry"), 2);
    assert_eq!(run.started("wait_to_retry"), 2);
    assert_eq!(run.started("accept_response"), 1);
    assert_eq!(run.started("abandon"), 0);
    assert_eq!(run.started("report_failure"), 0);

    assert_eq!(run.chosen(), ["retry", "retry"]);
    let first = &run.retry_decisions[0];
    assert!(first.consulted("attempt=1"), "{:?}", first.consulted);
    assert!(
        first.consulted(&format!("failure.kind={}", FailureKind::Overloaded)),
        "{:?}",
        first.consulted
    );
    assert!(first.consulted("committed=false"), "{:?}", first.consulted);
    assert!(run.retry_decisions[1].consulted("attempt=2"));
}

#[tokio::test]
async fn an_exhausted_budget_ends_in_the_give_up_branch_and_one_report() {
    let provider = Scripted::new(|_| Step::Fail(http(529, "overloaded_error", "Overloaded")));
    let run = drive(&provider, fast(3)).await;

    assert_eq!(provider.streams(), 3);
    assert_eq!(run.started("act"), 3);
    assert_eq!(run.started("decide_retry"), 3);
    assert_eq!(
        run.started("wait_to_retry"),
        2,
        "no backoff follows the last attempt"
    );
    assert_eq!(run.started("abandon"), 1);
    assert_eq!(run.started("report_failure"), 1);
    assert_eq!(run.started("accept_response"), 0);
    assert_eq!(run.chosen(), ["retry", "retry", "give-up"]);
    assert_eq!(
        run.error_rows(),
        std::slice::from_ref(&run.failure().message)
    );
}

#[tokio::test]
async fn a_failure_no_retry_can_fix_goes_straight_to_the_give_up_branch() {
    let provider =
        Scripted::new(|_| Step::Fail(http(401, "authentication_error", "invalid x-api-key")));
    let run = drive(&provider, fast(5)).await;

    assert_eq!(run.started("act"), 1);
    assert_eq!(run.started("wait_to_retry"), 0);
    assert_eq!(run.started("abandon"), 1);
    assert_eq!(run.chosen(), ["give-up"]);
    assert!(run.retry_decisions[0].consulted(&format!("failure.kind={}", FailureKind::Auth)));
}

#[tokio::test]
async fn a_failure_after_shown_text_is_recorded_as_committed() {
    let provider =
        Scripted::new(|_| Step::TextThenFail(GenerationError::Http("connection reset".into())));
    let run = drive(&provider, fast(5)).await;

    assert_eq!(run.chosen(), ["give-up"]);
    assert!(
        run.retry_decisions[0].consulted("committed=true"),
        "{:?}",
        run.retry_decisions[0].consulted
    );
    assert_eq!(run.started("wait_to_retry"), 0);
}

#[tokio::test]
async fn retries_are_not_counted_against_the_turn_limit() {
    // `--max-turns` bounds how many times the agent loop goes round. A model
    // call that needs four attempts is still one trip round it.
    let provider = Scripted::new(|_| Step::Fail(http(529, "overloaded_error", "Overloaded")));
    let setup = Setup {
        max_turns: 2,
        ..Setup::default()
    };
    let run = drive_with(&provider, fast(4), setup).await;

    assert_eq!(provider.streams(), 4, "the whole attempt budget was used");
    assert_eq!(run.failure().kind, FailureKind::Overloaded);
}
