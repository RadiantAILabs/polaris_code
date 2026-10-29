//! Proof that an operator-set spend limit actually stops a run: at a turn
//! boundary, cleanly, with the session saved, a distinguishable exit status,
//! and a statement of what was spent against what was allowed.
//!
//! The gap this closes: `TokenMeter` accounted for a run and nothing read that
//! accounting to decide whether to continue, so an agent in a loop could spend
//! without bound and the eval harness's ceiling had to be enforced from
//! outside the process.
//!
//! Everything here runs against a scripted `LlmProvider` that streams chosen
//! [`Usage`] values, so the exact spend is known in advance. No API key, no
//! network, no spend.
//!
//! The non-vacuity argument these tests rest on: [`ScriptedProvider`] counts
//! its own `stream` calls. A turn that the ceiling prevented is therefore
//! distinguishable from a turn that ran and produced nothing — the assertions
//! below read that counter rather than inferring "nothing happened" from an
//! absent side effect.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider, OutputFormat, RunReport};
use polaris_code::run::{self, CEILING_EXIT_CODE, TurnOutcome, TurnReason};
use polaris_code::usage::{RUN_CEILING_SCHEMA, RunCeiling, TokenMeter};
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The text the scripted model answers with, on every turn.
const ANSWER: &str = "done.";

/// A priced model id routed to [`ScriptedProvider`]: the provider segment
/// selects the scripted provider, and `sonnet` in the model segment is what
/// `price_for` matches a rate card on ($3/Mtok in, $15/Mtok out).
const SONNET: &str = "scripted/claude-sonnet-4-6";

/// A model id routed to the same provider that `price_for` has no rate for.
const UNPRICED: &str = "scripted/no-such-family";

/// What one scripted turn bills: 100k input + 10k output, which on the Sonnet
/// rate card is exactly $0.45 a turn and `110_000` tokens a turn.
const INPUT_PER_TURN: u64 = 100_000;
const OUTPUT_PER_TURN: u64 = 10_000;
const TOKENS_PER_TURN: u64 = INPUT_PER_TURN + OUTPUT_PER_TURN;
const USD_PER_TURN: f64 = 0.45;

/// The output token limit these tests report a turn as having run under.
const MAX_OUTPUT_TOKENS: u32 = 4096;

/// A [`PriceStore`] that answers only from its built-in fallback table —
/// deterministic dollar amounts (in particular, `USD_PER_TURN` above) with no
/// network access, per the `PriceStore` testing policy.
fn fallback_only_pricing_store() -> PriceStore {
    PriceStore::builtin()
}

/// An `LlmProvider` that streams one fixed answer and a fixed [`Usage`], and
/// counts how many turns actually reached it.
#[derive(Default)]
struct ScriptedProvider {
    streams: Arc<AtomicU64>,
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
        self.streams.fetch_add(1, Ordering::Relaxed);
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
                    input_tokens: Some(INPUT_PER_TURN),
                    output_tokens: Some(OUTPUT_PER_TURN),
                    cache_read_tokens: Some(0),
                    cache_creation_tokens: Some(0),
                    total_tokens: Some(TOKENS_PER_TURN),
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
    streams: Arc<AtomicU64>,
}

impl Plugin for ScriptedModelPlugin {
    const ID: &'static str = "test::scripted_ceiling_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before ScriptedModelPlugin")
            .register_llm_provider(ScriptedProvider {
                streams: self.streams.clone(),
            });
    }
}

/// One run assembled exactly as `main.rs` assembles a `--print` run: the
/// scripted model instead of `AnthropicPlugin`, [`ExecIOPlugin`] instead of the
/// TUI panels, the session [`TokenMeter`] as a global, and — when the operator
/// set one — the [`RunCeiling`] beside it, both before any context is created.
struct Host {
    server: Server,
    provider: Arc<ExecIOProvider>,
    input_tx: mpsc::Sender<IOMessage>,
    _output_rx: mpsc::Receiver<IOMessage>,
    meter: TokenMeter,
    streams: Arc<AtomicU64>,
    session_id: SessionId,
}

async fn host(
    data_dir: &Path,
    workspace: &Path,
    model_id: &str,
    ceiling: Option<RunCeiling>,
) -> Host {
    let (provider, input_tx, output_rx) = ExecIOProvider::new(64);
    let provider = Arc::new(provider);
    let streams = Arc::new(AtomicU64::new(0));
    let meter = TokenMeter::new().with_pricing_store(fallback_only_pricing_store());

    let mut server = Server::new();
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(ScriptedModelPlugin {
            streams: streams.clone(),
        })
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
            data_dir.to_str().expect("a UTF-8 temp path"),
        ))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.insert_global(meter.clone());
    if let Some(ceiling) = ceiling {
        server.insert_global(ceiling);
    }
    server.finish().await.expect("the server starts");

    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI");
    sessions.set_serializers(persistence.serializers());
    sessions
        .register_agent(CodeAgent)
        .expect("CodeAgent registers");

    let session_id = SessionId::new();
    let config = AgentConfig::new(model_id, workspace.to_path_buf());
    sessions
        .create_session_with_executor(
            server.create_context(),
            &session_id,
            &AgentTypeId::from_name(CodeAgent::NAME),
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .expect("the session is created");

    Host {
        server,
        provider,
        input_tx,
        _output_rx: output_rx,
        meter,
        streams,
        session_id,
    }
}

impl Host {
    fn sessions(&self) -> &SessionsAPI {
        self.server.api::<SessionsAPI>().expect("SessionsAPI")
    }

    /// How many turns actually reached the model.
    fn turns_run(&self) -> u64 {
        self.streams.load(Ordering::Relaxed)
    }

    /// Run the interactive driver's loop over at most `max_turns` prompts,
    /// stopping exactly where [`run::drive_until`] stops, and return every
    /// outcome in order.
    ///
    /// A prompt is enqueued before each turn, so a turn the ceiling refused is
    /// visible as a prompt left unconsumed and a `turns_run` that did not move.
    async fn drive_until(&self, max_turns: usize) -> Vec<TurnOutcome> {
        let mut outcomes = Vec::new();
        for turn in 0..max_turns {
            self.input_tx
                .send(IOMessage::from_external(
                    "cli",
                    IOContent::Text(format!("prompt {turn}")),
                ))
                .await
                .expect("the prompt is enqueued before the turn starts");
            let outcome = run::drive_turn(self.sessions(), &self.session_id).await;
            let stop = !run::driver_continues(&outcome.reason);
            outcomes.push(outcome);
            if stop {
                break;
            }
        }
        outcomes
    }

    /// Save the session the way both front ends do on the way out, and prove
    /// the store really has it afterwards.
    async fn save_and_reload(&self) -> Vec<SessionId> {
        self.sessions()
            .save_session(&self.session_id)
            .await
            .expect("the session saves cleanly after a ceiling stop");
        self.sessions()
            .list_sessions()
            .await
            .expect("the session store lists what it holds")
    }
}

fn ceiling(cost: Option<f64>, tokens: Option<u64>) -> RunCeiling {
    RunCeiling::from_limits(cost, tokens)
        .expect("valid limits")
        .expect("at least one limit")
}

#[tokio::test]
async fn a_run_stops_at_the_turn_boundary_once_its_spend_reaches_the_limit() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    // Two turns' worth of tokens exactly. Turn one is under, turn two reaches
    // it, turn three must never start.
    let limit = TOKENS_PER_TURN * 2;
    let host = host(
        data_dir.path(),
        workspace.path(),
        SONNET,
        Some(ceiling(None, Some(limit))),
    )
    .await;

    let outcomes = host.drive_until(5).await;

    assert_eq!(
        host.turns_run(),
        2,
        "the run took exactly the turns its budget paid for"
    );
    assert_eq!(
        outcomes.len(),
        2,
        "the driver stopped after the second turn"
    );

    let first = &outcomes[0];
    assert_eq!(first.reason, TurnReason::Completed);
    let first_ceiling = first.ceiling.as_ref().expect("a ceiling was set");
    assert!(!first_ceiling.stopped, "one turn is inside the budget");

    let last = &outcomes[1];
    assert_eq!(
        last.reason,
        TurnReason::CeilingReached,
        "the run ended because of the limit, not because it was finished"
    );
    // The boundary, not an interrupt: the turn that crossed the line ran to
    // completion and its work is still reported.
    assert_eq!(last.final_text.as_deref(), Some(ANSWER));
    assert!(last.run_id.is_some(), "the crossing turn really executed");
    assert_eq!(last.turn_number, 2);
    assert_eq!(host.meter.total_tokens(), limit);

    // The limit is named where the operator reads it.
    let report = RunReport::new(&host.session_id, last, MAX_OUTPUT_TOKENS);
    let line = report.ceiling_line().expect("the stop is stated");
    assert!(
        line.contains(&format!("of {limit} allowed")),
        "the limit itself is named: {line}"
    );
    assert!(
        line.contains(&format!("{} tokens", limit)),
        "what was spent is named: {line}"
    );

    // Cleanly: the session is saved and still there.
    let saved = host.save_and_reload().await;
    assert!(
        saved.contains(&host.session_id),
        "the stopped run's session is in the store: {saved:?}"
    );
    host.provider.close().await;
}

#[tokio::test]
async fn the_stop_has_its_own_exit_status_and_reports_spent_against_allowed() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    // A money limit one turn's spend reaches exactly.
    let host = host(
        data_dir.path(),
        workspace.path(),
        SONNET,
        Some(ceiling(Some(USD_PER_TURN), None)),
    )
    .await;

    let outcomes = host.drive_until(3).await;
    assert_eq!(host.turns_run(), 1);
    let last = outcomes.last().expect("one turn ran");
    let report = RunReport::new(&host.session_id, last, MAX_OUTPUT_TOKENS);

    assert_eq!(
        report.exit_code(),
        CEILING_EXIT_CODE,
        "a budget stop is distinguishable from every other ending"
    );
    assert_eq!(report.exit_code(), 3);
    assert_eq!(report.status, "ceiling-reached");
    assert_eq!(
        report.error, None,
        "reaching the limit the operator set is not a failure"
    );

    // Spent versus allowed, in the machine-readable object.
    let payload = report
        .stdout_payload(OutputFormat::Json)
        .expect("`json` always prints one object");
    let json: serde_json::Value =
        serde_json::from_str(&payload).expect("the `json` stdout object parses");
    let ceiling = &json["ceiling"];
    assert_eq!(ceiling["schema"], RUN_CEILING_SCHEMA);
    assert_eq!(ceiling["stopped"], true);
    assert_eq!(ceiling["max_cost_usd"].as_f64(), Some(USD_PER_TURN));
    assert_eq!(ceiling["max_tokens"], serde_json::Value::Null);
    let spent = ceiling["spent_usd"].as_f64().expect("a priced model");
    assert!(
        (spent - USD_PER_TURN).abs() < 1e-9,
        "one turn's spend is reported: {spent}"
    );
    assert_eq!(ceiling["spent_tokens"].as_u64(), Some(TOKENS_PER_TURN));
    assert!(
        ceiling["reason"]
            .as_str()
            .expect("the stop names its limit")
            .contains("$0.4500 of the $0.4500 allowed")
    );

    // The separate `polaris-code-run-usage/v1` object keeps its own meaning:
    // the ceiling is reported beside it, never folded into it.
    assert_eq!(json["usage"]["schema"], "polaris-code-run-usage/v1");
    assert!(
        json["usage"].get("max_cost_usd").is_none(),
        "the usage schema gained no limit field: {}",
        json["usage"]
    );
    assert_eq!(
        json["usage"]["estimated_cost_usd"].as_f64(),
        Some(spent),
        "the two objects cannot disagree about what was spent"
    );

    // The same statement on stderr, for the formats with no structured place
    // to put it.
    let line = report.ceiling_line().expect("the stop is stated");
    assert_eq!(
        line,
        "polaris-code: spend ceiling reached, stopping: $0.4500 of $0.4500 allowed"
    );
    host.provider.close().await;
}

#[tokio::test]
async fn an_unpriced_model_under_a_money_ceiling_is_stopped_not_unlimited() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    // A limit no realistic run would reach — if "unpriced" meant "unlimited",
    // this run would spend every turn it was offered.
    let generous = ceiling(Some(1_000.0), None);

    // Documented behaviour, layer one: the binary refuses at startup, because
    // the ceiling cannot be shown to hold. `main.rs` reports this through
    // `fatal`, which is exit 2 — a configuration error, not a turn failure.
    let why = generous
        .unenforceable_for(&fallback_only_pricing_store(), UNPRICED)
        .expect("an unpriced model cannot carry a money ceiling");
    assert!(why.contains("no price is known"), "{why}");
    assert!(why.contains("tokens instead"), "{why}");
    assert_ne!(CEILING_EXIT_CODE, 2, "refusing up front is not the stop");

    // Layer two, for a run that reached the loop anyway (a library caller, or
    // a model that becomes unpriced mid-session): the run stops rather than
    // running unbounded.
    let host = host(data_dir.path(), workspace.path(), UNPRICED, Some(generous)).await;
    let outcomes = host.drive_until(3).await;

    assert_eq!(
        host.turns_run(),
        0,
        "no turn starts under a ceiling that cannot be shown to hold"
    );
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].reason, TurnReason::CeilingReached);
    let status = outcomes[0].ceiling.as_ref().expect("a ceiling was set");
    assert_eq!(status.spent_usd, None, "not priced is not free");
    assert!(status.reason.as_deref().unwrap().contains("unpriced"));
    assert_eq!(
        RunReport::new(&host.session_id, &outcomes[0], MAX_OUTPUT_TOKENS).exit_code(),
        CEILING_EXIT_CODE
    );
    host.provider.close().await;
}

#[tokio::test]
async fn a_token_ceiling_bounds_an_unpriced_run_normally() {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let data_dir = tempfile::tempdir().expect("a temp session store");
    // The documented way to bound an unpriced model: count tokens, which needs
    // no rate card.
    let host = host(
        data_dir.path(),
        workspace.path(),
        UNPRICED,
        Some(ceiling(None, Some(TOKENS_PER_TURN))),
    )
    .await;

    let outcomes = host.drive_until(3).await;

    assert_eq!(host.turns_run(), 1, "the unpriced run did get its turn");
    assert_eq!(outcomes.last().unwrap().reason, TurnReason::CeilingReached);
    let report = RunReport::new(
        &host.session_id,
        outcomes.last().unwrap(),
        MAX_OUTPUT_TOKENS,
    );
    assert_eq!(
        report.ceiling_line().unwrap(),
        format!(
            "polaris-code: spend ceiling reached, stopping: \
             {TOKENS_PER_TURN} tokens of {TOKENS_PER_TURN} allowed"
        )
    );
    host.provider.close().await;
}

#[tokio::test]
async fn a_run_under_its_limit_behaves_and_reports_exactly_as_one_with_no_limit() {
    let workspace = tempfile::tempdir().expect("a temp workspace");

    // Same run twice: once with a limit a hundred turns away, once with none.
    let unlimited_dir = tempfile::tempdir().expect("a temp session store");
    let unlimited = host(unlimited_dir.path(), workspace.path(), SONNET, None).await;
    let unlimited_outcome = unlimited.drive_until(1).await.pop().expect("one turn");

    let bounded_dir = tempfile::tempdir().expect("a temp session store");
    let bounded = host(
        bounded_dir.path(),
        workspace.path(),
        SONNET,
        Some(ceiling(
            Some(USD_PER_TURN * 100.0),
            Some(TOKENS_PER_TURN * 100),
        )),
    )
    .await;
    let bounded_outcome = bounded.drive_until(1).await.pop().expect("one turn");

    // Behaviour: identical.
    assert_eq!(unlimited.turns_run(), 1);
    assert_eq!(bounded.turns_run(), 1);
    assert_eq!(bounded_outcome.reason, unlimited_outcome.reason);
    assert_eq!(bounded_outcome.reason, TurnReason::Completed);
    assert_eq!(bounded_outcome.final_text, unlimited_outcome.final_text);
    assert_eq!(bounded_outcome.turn_number, unlimited_outcome.turn_number);
    assert_eq!(bounded.meter.total_tokens(), unlimited.meter.total_tokens());

    let unlimited_report =
        RunReport::new(&unlimited.session_id, &unlimited_outcome, MAX_OUTPUT_TOKENS);
    let bounded_report = RunReport::new(&bounded.session_id, &bounded_outcome, MAX_OUTPUT_TOKENS);
    assert_eq!(bounded_report.exit_code(), 0);
    assert_eq!(bounded_report.exit_code(), unlimited_report.exit_code());
    assert_eq!(bounded_report.status, unlimited_report.status);
    assert_eq!(bounded_report.status, "completed");
    assert_eq!(
        bounded_report.ceiling_line(),
        None,
        "a run inside its limit says nothing about it"
    );
    assert_eq!(unlimited_report.ceiling_line(), None);

    // Output shape: the same keys either way, and the `usage` object is
    // untouched by the presence of a limit.
    let object = |report: &RunReport| -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(
            &report
                .stdout_payload(OutputFormat::Json)
                .expect("`json` always prints one object"),
        )
        .expect("parses")
        .as_object()
        .expect("the report is an object")
        .clone()
    };
    let bounded_json = object(&bounded_report);
    let unlimited_json = object(&unlimited_report);
    assert_eq!(
        bounded_json.keys().collect::<Vec<_>>(),
        unlimited_json.keys().collect::<Vec<_>>(),
        "a limit adds no key and removes none"
    );
    assert_eq!(bounded_json["usage"], unlimited_json["usage"]);
    assert_eq!(
        unlimited_json["ceiling"],
        serde_json::Value::Null,
        "no limit set reports null, which is not `within the limit`"
    );
    // The run that has a limit reports where it stands against it, so a user
    // can watch it approach rather than only learn about it at the stop.
    let standing = bounded_outcome.ceiling.as_ref().expect("a ceiling was set");
    assert!(!standing.stopped);
    assert!(
        standing.used_fraction().expect("a fraction of a limit") < 0.02,
        "one turn is nowhere near a hundred turns' budget"
    );
    assert_eq!(bounded_json["ceiling"]["stopped"], false);

    unlimited.provider.close().await;
    bounded.provider.close().await;
}
