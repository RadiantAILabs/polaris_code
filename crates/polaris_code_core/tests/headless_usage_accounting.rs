//! Proof that a headless run reports accounting a consumer can trust: the
//! numbers are the ones the run itself billed, the shape is versioned, and
//! every machine-readable output format carries the same object.
//!
//! The gap this closes: `RunReport` carried no token or cost field at all, the non-interactive
//! front end never read the [`TokenMeter`] (only the interactive path printed
//! it, after the TUI tore down), and the `stream-json` surface exposed only
//! `io_type = "tokens"` messages carrying `in`/`out` *deltas* as strings — no
//! cache tiers, no cost, no model identity, no pricing basis.
//!
//! Everything here runs against a scripted `LlmProvider` that streams chosen
//! [`Usage`] values, so the exact numbers the provider reports are known and
//! can be compared with the numbers the run emits. No API key, no network, no
//! spend.
//!
//! The non-vacuity argument these tests rest on: the agent's `act` system
//! takes `Res<TokenMeter>` and is the only thing that bills the meter, while
//! the recovery path (`recover`) uses a non-streaming `generate` and bills
//! nothing. So a non-zero counter here can only have come from `act` actually
//! running and folding the streamed usage in — not from a default, a constant,
//! or the error path.

use std::path::Path;
use std::sync::Arc;

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, StopReason, StreamEvent, Usage,
};
use polaris::models::pricing::PriceStore;
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, IOProvider, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code_core::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code_core::exec::{
    ExecIOPlugin, ExecIOProvider, OutputFormat, RunReport, USAGE_IO_TYPE, stream_json_line,
    usage_message,
};
use polaris_code_core::run;
use polaris_code_core::usage::{PRICE_BASIS, RUN_USAGE_SCHEMA, TokenMeter, price_for};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The text the scripted model answers with, on every turn.
const ANSWER: &str = "done.";

/// A priced model id routed to [`ScriptedProvider`]: the provider segment
/// selects the scripted provider, and `sonnet` in the model segment is what
/// `price_for` matches a rate card on.
const SONNET: &str = "scripted/claude-sonnet-4-6";

/// A model id routed to the same provider that `price_for` has no rate for.
const UNPRICED: &str = "scripted/no-such-family";

/// The output token limit these runs report. Deliberately not 4096, so an
/// assertion on it cannot pass by matching the provider default by accident.
const MAX_OUTPUT_TOKENS: u32 = 32_000;

/// A [`PriceStore`] pinned to the built-in fallback table, for tests that
/// assert on exact rates or need pricing without touching the network.
fn fallback_only_pricing_store() -> PriceStore {
    PriceStore::builtin()
}

/// The four billed token counts one scripted turn reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Billed {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_creation: u64,
}

impl Billed {
    const fn total(self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_creation
    }
}

/// An `LlmProvider` that streams one fixed answer and the exact [`Usage`] it
/// was built with, so a test knows precisely what the run should have billed.
struct ScriptedProvider {
    billed: Billed,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        Ok(LlmResponse {
            content: vec![AssistantBlock::text(ANSWER)],
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
        use futures_util::StreamExt as _;
        let billed = self.billed;
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text(ANSWER.to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: Usage {
                    input_tokens: Some(billed.input),
                    output_tokens: Some(billed.output),
                    cache_read_tokens: Some(billed.cache_read),
                    cache_creation_tokens: Some(billed.cache_creation),
                    total_tokens: Some(billed.total()),
                    ..Default::default()
                },
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

/// Registers [`ScriptedProvider`] with [`ModelRegistry`], the same seam
/// `AnthropicPlugin` uses in production.
struct ScriptedModelPlugin {
    billed: Billed,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_usage_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(ScriptedProvider {
                billed: self.billed,
            });
    }
}

/// One headless run, assembled exactly as `main.rs`'s `--print` branch does:
/// the scripted model instead of `AnthropicPlugin`, [`ExecIOPlugin`] instead
/// of the TUI panels, and the session [`TokenMeter`] inserted as a global
/// before any context is created.
struct Headless {
    server: Server,
    provider: Arc<ExecIOProvider>,
    input_tx: mpsc::Sender<IOMessage>,
    output_rx: mpsc::Receiver<IOMessage>,
    meter: Option<TokenMeter>,
}

/// What one driven turn produced: the report a front end would emit, the
/// `stream-json` lines a `--output-format stream-json` run would have printed,
/// and the meter the run billed (when one was installed).
struct Ran {
    report: RunReport,
    stream_json_lines: Vec<String>,
    meter: Option<TokenMeter>,
}

impl Ran {
    /// The `json` format's stdout object, parsed the way a consumer parses it.
    fn json(&self) -> serde_json::Value {
        let payload = self
            .report
            .stdout_payload(OutputFormat::Json)
            .expect("`json` always prints one object");
        serde_json::from_str(&payload).expect("the `json` stdout object parses")
    }

    /// Every `stream-json` line, parsed back into the [`IOMessage`] a consumer
    /// reads off the stream.
    fn stream_messages(&self) -> Vec<IOMessage> {
        self.stream_json_lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("each stream-json line is one IOMessage"))
            .collect()
    }
}

async fn headless(data_dir: &Path, billed: Billed, meter: Option<TokenMeter>) -> Headless {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);

    let mut server = Server::new();
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin { billed })
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    // `main.rs` registers the meter as a global before any `create_context()`,
    // which is what puts it in reach of the agent's `act` system and of the
    // run surface reading it back afterwards.
    if let Some(meter) = meter.clone() {
        server.insert_global(meter);
    }
    server.finish().await.expect("the server starts");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    Headless {
        server,
        provider,
        input_tx,
        output_rx,
        meter,
    }
}

impl Headless {
    /// Creates `session_id` under `model_id`, exactly as the `--print` branch
    /// does before driving its one turn.
    fn create(&self, session_id: &SessionId, model_id: &str, workspace: &Path) {
        let config = AgentConfig::new(model_id, workspace.to_path_buf());
        self.sessions()
            .create_session_with_executor(
                self.server.create_context(),
                session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
                move |ctx| {
                    ctx.insert(config.clone());
                },
            )
            .expect("the session is created");
    }

    fn sessions(&self) -> &SessionsAPI {
        self.server.api::<SessionsAPI>().expect("SessionsAPI")
    }

    /// Drives one turn and renders the result the way `run_headless` does for
    /// `format`: drain concurrently, drive, then — for a streaming format —
    /// push the run's accounting through the same provider as the closing
    /// message, so the lines collected here are byte-identical to what the
    /// binary prints.
    async fn drive(self, session_id: &SessionId, format: OutputFormat, prompt: &str) -> Ran {
        let Self {
            server,
            provider,
            input_tx,
            mut output_rx,
            meter,
        } = self;
        input_tx
            .send(IOMessage::from_external(
                "cli",
                IOContent::Text(prompt.into()),
            ))
            .await
            .expect("the prompt is enqueued before the turn starts");
        drop(input_tx);

        let drain = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Some(message) = output_rx.recv().await {
                if format.streams_messages() {
                    lines.push(stream_json_line(&message).expect("an IOMessage serializes"));
                }
            }
            lines
        });

        let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
        let outcome = run::drive_turn(sessions, session_id).await;

        if format.streams_messages()
            && let Some(usage) = outcome.usage.as_ref()
        {
            provider
                .send(usage_message(usage))
                .await
                .expect("the closing usage message is sent");
        }
        provider.close().await;
        let stream_json_lines = drain.await.expect("the drain task finishes");

        Ran {
            report: RunReport::new(session_id, &outcome, MAX_OUTPUT_TOKENS),
            stream_json_lines,
            meter,
        }
    }
}

/// Runs one whole headless turn end to end and returns what it emitted.
async fn run_once(billed: Billed, model_id: &str, format: OutputFormat) -> Ran {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let meter = TokenMeter::new().with_pricing_store(fallback_only_pricing_store());
    let headless = headless(data_dir.path(), billed, Some(meter)).await;
    let session_id = SessionId::new();
    headless.create(&session_id, model_id, workspace.path());
    headless.drive(&session_id, format, "do the thing").await
}

/// The emitted counters, read out of the `json` stdout object the way a
/// consumer reads them.
fn emitted_counts(usage: &serde_json::Value) -> Billed {
    let field = |name: &str| {
        usage[name]
            .as_u64()
            .unwrap_or_else(|| panic!("`{name}` must be an integer, got {}", usage[name]))
    };
    Billed {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cache_read: field("cache_read_tokens"),
        cache_creation: field("cache_creation_tokens"),
    }
}

#[tokio::test]
async fn the_emitted_accounting_is_the_runs_own_measurement_not_a_constant() {
    // Two runs that differ in every counter. A hardcoded value, a zero
    // default, or a number recomputed from the transcript would match at most
    // one of them.
    let first = Billed {
        input: 1_234,
        output: 56,
        cache_read: 789,
        cache_creation: 12,
    };
    let second = Billed {
        input: 7,
        output: 8_888,
        cache_read: 9,
        cache_creation: 10_101,
    };
    assert_ne!(
        first, second,
        "the two runs must differ to be discriminating"
    );

    for billed in [first, second] {
        let ran = run_once(billed, SONNET, OutputFormat::Json).await;
        let json = ran.json();
        let usage = &json["usage"];

        assert_eq!(
            emitted_counts(usage),
            billed,
            "the emitted counters must be what this run billed: {usage}"
        );

        // Non-zero counters can only come from `act` folding the streamed
        // usage into the meter — the recovery path bills nothing — so this
        // also proves the run went through the real agent step.
        assert!(billed.input > 0 && usage["input_tokens"].as_u64() == Some(billed.input));
        assert_eq!(
            usage["total_tokens"].as_u64(),
            Some(billed.total()),
            "the emitted total must be the sum of the emitted parts"
        );

        // The same numbers the meter itself holds: the report is a read of the
        // run's accounting, not a parallel tally.
        let meter = ran.meter.as_ref().expect("this run installed a meter");
        assert_eq!(meter.input_tokens(), billed.input);
        assert_eq!(meter.output_tokens(), billed.output);
        assert_eq!(meter.cache_read_tokens(), billed.cache_read);
        assert_eq!(meter.cache_creation_tokens(), billed.cache_creation);
        assert_eq!(meter.total_tokens(), billed.total());

        // And the cost is the meter's own estimate for the model the run drove,
        // not a second pricing calculation.
        let expected_cost = meter
            .estimated_cost_usd(SONNET)
            .expect("the sonnet family is priced");
        let emitted_cost = usage["estimated_cost_usd"]
            .as_f64()
            .expect("a priced run reports a cost");
        assert!(
            (emitted_cost - expected_cost).abs() < 1e-12,
            "emitted {emitted_cost} should be the meter's own {expected_cost}"
        );
        assert!(emitted_cost > 0.0, "a run that billed tokens costs money");
    }
}

#[tokio::test]
async fn the_cache_tiers_the_streaming_surface_never_exposed_are_reported() {
    // The live `io_type = "tokens"` messages carry `in`/`out` only. A consumer
    // summing them would miss both cache tiers entirely — here they are more
    // than half the billed tokens, so the difference is not a rounding detail.
    let billed = Billed {
        input: 100,
        output: 20,
        cache_read: 5_000,
        cache_creation: 800,
    };
    let ran = run_once(billed, SONNET, OutputFormat::StreamJson).await;

    let messages = ran.stream_messages();
    let token_messages: Vec<&IOMessage> = messages
        .iter()
        .filter(|m| m.metadata.get("io_type").map(String::as_str) == Some("tokens"))
        .collect();
    assert!(
        !token_messages.is_empty(),
        "the live token deltas still stream for the panel"
    );
    for message in &token_messages {
        assert!(
            !message.metadata.contains_key("cache_read")
                && !message.metadata.contains_key("cache_creation"),
            "the live deltas are a display signal and carry no cache tiers: {:?}",
            message.metadata
        );
    }

    // The accounting message does carry them, as integers rather than strings.
    let usage = closing_usage(&messages);
    assert_eq!(usage["cache_read_tokens"].as_u64(), Some(billed.cache_read));
    assert_eq!(
        usage["cache_creation_tokens"].as_u64(),
        Some(billed.cache_creation)
    );

    // Summing the live in/out deltas is not the answer, which is exactly why a
    // consumer must not do it.
    let delta_sum: u64 = token_messages
        .iter()
        .filter_map(|m| {
            let get = |k: &str| m.metadata.get(k)?.parse::<u64>().ok();
            Some(get("in")? + get("out")?)
        })
        .sum();
    assert_ne!(
        delta_sum,
        billed.total(),
        "the live deltas must not accidentally equal the billed total, or this \
         test would not distinguish the two sources"
    );
}

/// The one closing accounting message on a `stream-json` stream, parsed.
fn closing_usage(messages: &[IOMessage]) -> serde_json::Value {
    let usage_messages: Vec<&IOMessage> = messages
        .iter()
        .filter(|m| m.metadata.get("io_type").map(String::as_str) == Some(USAGE_IO_TYPE))
        .collect();
    assert_eq!(
        usage_messages.len(),
        1,
        "exactly one accounting message per run, so a consumer never has to \
         choose between two"
    );
    let IOContent::Text(text) = &usage_messages[0].content else {
        panic!("the accounting message carries its object as text");
    };
    serde_json::from_str(text).expect("the accounting message's text is one JSON object")
}

#[tokio::test]
async fn both_machine_readable_formats_carry_the_same_versioned_object() {
    let billed = Billed {
        input: 321,
        output: 45,
        cache_read: 67,
        cache_creation: 89,
    };

    let json_run = run_once(billed, SONNET, OutputFormat::Json).await;
    let stream_run = run_once(billed, SONNET, OutputFormat::StreamJson).await;

    let from_json = json_run.json()["usage"].clone();
    let messages = stream_run.stream_messages();
    let from_stream = closing_usage(&messages);

    // One contract, one parser: what `json` embeds and what `stream-json`
    // closes with are the same object.
    assert_eq!(
        from_json, from_stream,
        "the two machine-readable formats must not diverge"
    );

    // Versioned, so a consumer can tell which contract it is holding and
    // notice a later change instead of misreading it.
    assert_eq!(from_json["schema"].as_str(), Some(RUN_USAGE_SCHEMA));
    assert_eq!(
        from_json["schema"].as_str(),
        Some("polaris-code-run-usage/v1")
    );

    // Every field the contract promises is present and typed.
    assert_eq!(from_json["model_id"].as_str(), Some(SONNET));
    assert_eq!(emitted_counts(&from_json), billed);
    assert_eq!(from_json["total_tokens"].as_u64(), Some(billed.total()));
    assert!(from_json["estimated_cost_usd"].as_f64().is_some());
    assert_eq!(from_json["pricing_basis"].as_str(), Some(PRICE_BASIS));

    // The cost is exactly the emitted counts at the emitted rates, so the
    // evidence is self-checking: a consumer can verify it without this crate.
    let rate = |name: &str| {
        from_json["pricing"][name]
            .as_f64()
            .unwrap_or_else(|| panic!("`{name}` must be a rate"))
    };
    let recomputed = (billed.input as f64 / 1e6) * rate("input_per_mtok")
        + (billed.output as f64 / 1e6) * rate("output_per_mtok")
        + (billed.cache_read as f64 / 1e6) * rate("cache_read_per_mtok")
        + (billed.cache_creation as f64 / 1e6) * rate("cache_write_per_mtok");
    let emitted = from_json["estimated_cost_usd"].as_f64().expect("a cost");
    assert!(
        (emitted - recomputed).abs() < 1e-12,
        "emitted {emitted} should price the emitted counters at the emitted rates ({recomputed})"
    );
}

#[tokio::test]
async fn the_accounting_is_the_last_line_of_a_stream_json_run() {
    let billed = Billed {
        input: 11,
        output: 22,
        cache_read: 33,
        cache_creation: 44,
    };
    let ran = run_once(billed, SONNET, OutputFormat::StreamJson).await;

    assert!(
        ran.stream_json_lines.len() > 1,
        "the run streamed the turn's messages as well as the accounting"
    );
    let messages = ran.stream_messages();
    let last = messages.last().expect("at least one message");
    assert_eq!(
        last.metadata.get("io_type").map(String::as_str),
        Some(USAGE_IO_TYPE),
        "the accounting closes the stream, so a consumer knows the run is over"
    );

    // `stream-json` has already printed everything; nothing is emitted after.
    assert_eq!(ran.report.stdout_payload(OutputFormat::StreamJson), None);
    assert_eq!(ran.report.exit_code(), 0);
}

#[tokio::test]
async fn an_unpriced_model_reports_no_cost_but_still_names_the_basis_consulted() {
    let billed = Billed {
        input: 500,
        output: 60,
        cache_read: 7,
        cache_creation: 0,
    };
    assert!(
        price_for(&fallback_only_pricing_store(), UNPRICED).is_none(),
        "this test needs a model the price table does not know"
    );

    let ran = run_once(billed, UNPRICED, OutputFormat::Json).await;
    let json = ran.json();
    let usage = &json["usage"];

    // Measurement survives even when pricing does not.
    assert_eq!(emitted_counts(usage), billed);
    assert_eq!(usage["model_id"].as_str(), Some(UNPRICED));

    // `null` means *not priced*, and is distinguishable from a zero cost.
    assert!(
        usage["estimated_cost_usd"].is_null(),
        "an unpriced family must not report a fabricated number: {usage}"
    );
    assert!(usage["pricing"].is_null());

    // The basis is still named, so "unpriced" is attributable to a table.
    assert_eq!(usage["pricing_basis"].as_str(), Some(PRICE_BASIS));
    assert_eq!(usage["schema"].as_str(), Some(RUN_USAGE_SCHEMA));
}

#[tokio::test]
async fn an_unmeasured_run_reports_null_rather_than_zero() {
    // No meter installed: the run has no accounting to report, and says so,
    // rather than claiming it billed nothing.
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let billed = Billed {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_creation: 0,
    };
    let headless = headless(data_dir.path(), billed, None).await;
    let session_id = SessionId::new();
    headless.create(&session_id, SONNET, workspace.path());
    let ran = headless
        .drive(&session_id, OutputFormat::Json, "do the thing")
        .await;

    let json = ran.json();
    assert!(
        json["usage"].is_null(),
        "an unmeasured run reports null, not a zeroed object: {json}"
    );
    assert!(
        json.get("usage").is_some(),
        "the key is always present, so a consumer can tell unmeasured from an \
         older contract that had no accounting at all"
    );
}

#[tokio::test]
async fn a_run_that_never_reached_a_session_reports_no_accounting() {
    // Driving a session that was never created fails before the executor, so
    // there is no meter and no turn to account for. The report still parses
    // and still names its failure.
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let billed = Billed {
        input: 1,
        output: 1,
        cache_read: 1,
        cache_creation: 1,
    };
    let headless = headless(data_dir.path(), billed, Some(TokenMeter::new())).await;
    let session_id = SessionId::new();

    let outcome = run::drive_turn(headless.sessions(), &session_id).await;
    assert!(matches!(outcome.reason, run::TurnReason::Failed(_)));
    assert!(
        outcome.usage.is_none(),
        "a run that never addressed a session has nothing to report"
    );

    let report = RunReport::new(&session_id, &outcome, MAX_OUTPUT_TOKENS);
    let json: serde_json::Value = serde_json::from_str(
        &report
            .stdout_payload(OutputFormat::Json)
            .expect("`json` always prints one object"),
    )
    .expect("the object parses");
    assert_eq!(json["status"].as_str(), Some("failed"));
    assert!(json["usage"].is_null());
    // The limit is configuration, not accounting, so an unmeasured run still
    // states what bounded it.
    assert_eq!(json["max_output_tokens"].as_u64(), Some(32_000));
    assert_eq!(report.exit_code(), 1);
    headless.provider.close().await;
}

#[tokio::test]
async fn a_second_turn_reports_what_the_run_has_billed_so_far() {
    // The meter accumulates across the turns of one run, and the reported
    // numbers follow it rather than being re-derived per call. A `--print` run
    // drives exactly one turn, so for a headless run this total *is* the run's
    // total; documenting the cumulative semantics is what lets a consumer
    // reason about the interactive case too.
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let billed = Billed {
        input: 200,
        output: 30,
        cache_read: 40,
        cache_creation: 5,
    };
    let meter = TokenMeter::new().with_pricing_store(fallback_only_pricing_store());
    let headless = headless(data_dir.path(), billed, Some(meter.clone())).await;
    let session_id = SessionId::new();
    headless.create(&session_id, SONNET, workspace.path());
    let sessions = headless.sessions();

    // Turn one.
    headless
        .input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("first".into()),
        ))
        .await
        .expect("first prompt enqueued");
    let first = run::drive_turn(sessions, &session_id).await;
    let first_usage = first.usage.as_ref().expect("the run is measured");
    assert_eq!(first_usage.input_tokens, billed.input);

    // Turn two, same session and same meter.
    headless
        .input_tx
        .send(IOMessage::from_external(
            "cli",
            IOContent::Text("second".into()),
        ))
        .await
        .expect("second prompt enqueued");
    let second = run::drive_turn(sessions, &session_id).await;
    let second_usage = second.usage.as_ref().expect("the run is measured");

    assert_eq!(second.turn_number, 2, "the second turn really ran");
    assert_eq!(second_usage.input_tokens, billed.input * 2);
    assert_eq!(second_usage.total_tokens, billed.total() * 2);
    assert_eq!(meter.input_tokens(), billed.input * 2);
    let first_cost = first_usage.estimated_cost_usd.expect("priced");
    let second_cost = second_usage.estimated_cost_usd.expect("priced");
    assert!(
        second_cost > first_cost,
        "the second turn's report costs more than the first: {second_cost} vs {first_cost}"
    );
    headless.provider.close().await;
}

#[test]
fn the_pricing_basis_and_its_rates_move_together() {
    // The drift guard behind `pricing_basis`. Recorded evidence says which
    // rates produced its cost by naming this basis, so a rate may not change
    // silently underneath it.
    //
    // CHANGING THE BASIS NAME BELOW? Bump `PRICE_BASIS` in `src/usage.rs` and
    // update this test in the same commit. That is the whole contract: one
    // basis string names the store, whatever rates it answers with.
    assert_eq!(
        PRICE_BASIS, "litellm-price-store",
        "the basis identity is part of the emitted contract"
    );

    // Pinned to the built-in fallback table, so these rates cannot drift with
    // a live fetch: they are the last-resort rates the store falls back to
    // when the network and the cache are both unavailable.
    let store = fallback_only_pricing_store();
    for (model_id, input, output) in [
        ("anthropic/claude-opus-4", 15.0, 75.0),
        ("anthropic/claude-sonnet-4-6", 3.0, 15.0),
        ("anthropic/claude-haiku-4-5", 1.0, 5.0),
    ] {
        let price = price_for(&store, model_id).expect("a known family is priced");
        assert!((price.input_per_mtok - input).abs() < 1e-12, "{model_id}");
        assert!((price.output_per_mtok - output).abs() < 1e-12, "{model_id}");
        // Cache tiers are the store's own default ratios of input: 0.1x to
        // read, 1.25x to write a 5-minute-ephemeral entry. Exact equality,
        // not a tolerance, against that same formula: a silent change to
        // either multiplier must fail this test.
        assert_eq!(price.cache_read_per_mtok, input * 0.1, "{model_id}");
        assert_eq!(price.cache_write_per_mtok, input * 1.25, "{model_id}");
        for (rate, expected) in [
            (price.cache_read_per_mtok, input * 0.1),
            (price.cache_write_per_mtok, input * 1.25),
        ] {
            assert_eq!(
                serde_json::to_string(&rate).expect("a rate serializes"),
                format!("{expected:?}"),
                "the emitted rate card for {model_id} must serialize the same value it was computed as"
            );
        }
    }

    // The schema identity is equally part of the contract: a consumer pins it.
    assert_eq!(RUN_USAGE_SCHEMA, "polaris-code-run-usage/v1");
}
