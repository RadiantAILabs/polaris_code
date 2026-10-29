//! Consumer proof that an interactive session does not lose work.
//!
//! Four independent ways the interactive host used to drop a conversation on
//! the floor, each reproduced against the real coding composition —
//! [`CodeAgent`], tools, persistence, the ontrace store and a filesystem
//! [`FileStore`] — with only the model provider scripted:
//!
//! 1. the process ends without reaching its exit path, and every turn since
//!    launch goes with it;
//! 2. the exit save names the session the process launched with rather than
//!    the one a `/new` or `/resume` put in force;
//! 3. one recoverable turn failure ends the driver, leaving a TUI that still
//!    accepts input nothing will ever consume;
//! 4. a session that cannot serve *any* turn spins instead of stopping.
//!
//! The counterweight to (3) is tested too: a driver that never gives up is a
//! worse bug than one that gives up too early.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, GenerationError, LlmProvider,
    LlmRequest, LlmResponse, LlmStream, Message, StopReason, StreamEvent, ToolCall, Usage,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{IOContent, IOMessage, PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, ContextManager};
use polaris_code::exec::{ExecIOPlugin, ExecIOProvider};
use polaris_code::run;
use polaris_code::terminal::SessionNavigation;
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use tokio::sync::mpsc;

/// The answer the scripted model gives when it is behaving.
const ANSWER: &str = "durable answer";

/// Long enough that a correct host always wins, short enough that a wrong one
/// fails the test rather than hanging the suite.
const PATIENCE: Duration = Duration::from_secs(10);

/// The grace the composition root passes to `shutdown_driver`.
const GRACE: Duration = Duration::from_secs(2);

/// One model, two behaviours, switchable between turns.
///
/// Answering: a single text block. Runaway: a tool call every time, which the
/// `ReAct` loop keeps following until the executor's iteration ceiling stops
/// it — the real shape of the recoverable turn failure this host has to
/// survive.
struct SwitchableProvider {
    runaway: Arc<AtomicBool>,
}

impl SwitchableProvider {
    fn answer() -> LlmResponse {
        LlmResponse {
            content: vec![AssistantBlock::text(ANSWER)],
            usage: Usage::default(),
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        }
    }
}

impl LlmProvider for SwitchableProvider {
    fn name(&self) -> &'static str {
        "resilience-scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        if self.runaway.load(Ordering::SeqCst) {
            return Ok(LlmResponse {
                content: vec![AssistantBlock::ToolCall(ToolCall::new(
                    "runaway",
                    "list_files",
                    serde_json::json!({}),
                ))],
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                id: None,
                model: None,
            });
        }
        Ok(Self::answer())
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        use futures_util::StreamExt as _;
        let events = if self.runaway.load(Ordering::SeqCst) {
            vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::ToolCall {
                        id: "runaway".into(),
                        call_id: None,
                        name: "list_files".into(),
                    },
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: "{}".into(),
                    },
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::ContentBlockStart {
                    index: 0,
                    block: ContentBlockStartData::Text,
                }),
                Ok(StreamEvent::ContentBlockDelta {
                    index: 0,
                    delta: ContentBlockDelta::Text(ANSWER.to_owned()),
                }),
                Ok(StreamEvent::ContentBlockStop { index: 0 }),
                Ok(StreamEvent::MessageStop {
                    stop_reason: StopReason::EndTurn,
                    usage: Usage::default(),
                    id: None,
                    model: None,
                }),
            ]
        };
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct SwitchableModelPlugin(Arc<AtomicBool>);

impl Plugin for SwitchableModelPlugin {
    const ID: &'static str = "test::resilience_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin builds first")
            .register_llm_provider(SwitchableProvider {
                runaway: self.0.clone(),
            });
    }
}

/// The real coding host, minus the terminal.
struct Host {
    server: Server,
    provider: Arc<ExecIOProvider>,
    input: mpsc::Sender<IOMessage>,
    transcript: Transcript,
    runaway: Arc<AtomicBool>,
    _workspace: tempfile::TempDir,
}

/// Everything the front end was sent, as plain text.
#[derive(Clone, Default)]
struct Transcript(Arc<std::sync::Mutex<Vec<String>>>);

impl Transcript {
    fn push(&self, message: &IOMessage) {
        if let IOContent::Text(text) = &message.content {
            self.0.lock().expect("transcript lock").push(text.clone());
        }
    }

    fn contains(&self, needle: &str) -> bool {
        self.0
            .lock()
            .expect("transcript lock")
            .iter()
            .any(|line| line.contains(needle))
    }

    fn lines(&self) -> Vec<String> {
        self.0.lock().expect("transcript lock").clone()
    }

    /// Wait until some line contains `needle`, or give up after [`PATIENCE`].
    async fn wait_for(&self, needle: &str) -> bool {
        poll_until(|| self.contains(needle)).await
    }
}

/// Poll `condition` on a short tick until it holds or [`PATIENCE`] elapses.
///
/// The driver runs in its own task, so every assertion about it is an
/// assertion about something that has become true, not something that is true
/// the instant a send returns.
async fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

/// Build the coding host over `data_dir`.
///
/// The `ReAct` iteration ceiling belongs to the session's executor rather than
/// the host, so it is chosen per session in [`Host::create`]; the tests that
/// want a runaway turn set it low, so the ceiling is reached in fractions of a
/// second rather than after 25 scripted round trips.
async fn build_host(data_dir: &Path) -> Host {
    let workspace = tempfile::tempdir().expect("a temp workspace");
    let (provider, input, mut output) = ExecIOProvider::new(512);
    let provider = Arc::new(provider);
    let runaway = Arc::new(AtomicBool::new(false));

    let mut server = Server::new();
    server.insert_global(TokenMeter::new());
    server
        .add_plugins(ModelsPlugin)
        .add_plugins(SwitchableModelPlugin(runaway.clone()))
        .add_plugins(ToolsPlugin)
        .add_plugins(OxigraphPlugin::in_memory())
        .add_plugins(PersistencePlugin)
        .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(data_dir))))
        .add_plugins(CodeAgentPlugin::new())
        .add_plugins(ExecIOPlugin::new(provider.clone()));
    server.finish().await.expect("the coding host finishes");

    let sessions = server.api::<SessionsAPI>().expect("SessionsAPI").clone();
    let persistence = server.api::<PersistenceAPI>().expect("PersistenceAPI");
    sessions.set_serializers(persistence.serializers());
    sessions.register_agent(CodeAgent).expect("agent registers");

    let transcript = Transcript::default();
    let drain = transcript.clone();
    tokio::spawn(async move {
        while let Some(message) = output.recv().await {
            drain.push(&message);
        }
    });

    Host {
        server,
        provider,
        input,
        transcript,
        runaway,
        _workspace: workspace,
    }
}

impl Host {
    fn sessions(&self) -> SessionsAPI {
        self.server
            .api::<SessionsAPI>()
            .expect("SessionsAPI")
            .clone()
    }

    fn status(&self) -> run::StatusEmitter {
        run::StatusEmitter::new(self.provider.clone())
    }

    fn config(&self) -> AgentConfig {
        AgentConfig::new("resilience-scripted/model", self._workspace.path())
    }

    fn create(&self, id: &SessionId, max_iterations: usize) {
        let config = self.config();
        self.sessions()
            .create_session_with_executor(
                self.server.create_context(),
                id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(max_iterations),
                move |ctx| {
                    ctx.insert(config.clone());
                },
            )
            .expect("the coding session is created");
    }

    async fn submit(&self, prompt: &str) {
        self.input
            .send(IOMessage::from_external(
                "resilience-proof",
                IOContent::Text(prompt.to_owned()),
            ))
            .await
            .expect("the input channel stays open");
    }
}

/// Read a session back the way a later process would: a second host over the
/// same store directory, resuming from disk.
async fn recover_history(data_dir: &Path, id: &SessionId) -> Vec<Message> {
    let host = build_host(data_dir).await;
    let sessions = host.sessions();
    let config = host.config();
    sessions
        .resume_session_with_executor(
            host.server.create_context(),
            id,
            polaris::graph::GraphExecutor::new().with_default_max_iterations(25),
            move |ctx| {
                ctx.insert(config.clone());
            },
        )
        .await
        .unwrap_or_else(|error| {
            panic!(
                "session `{}` was not recoverable from {}: {error}",
                id.as_str(),
                data_dir.display()
            )
        });
    sessions
        .with_context(id, |ctx| {
            ctx.get_resource::<ContextManager>()
                .expect("a resumed coding session has a history")
                .messages
                .clone()
        })
        .await
        .expect("the resumed session is addressable")
}

/// Whether the store holds anything at all for `id`.
fn stored(data_dir: &Path, id: &SessionId) -> bool {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        entry
            .path()
            .file_stem()
            .is_some_and(|stem| stem == id.as_str())
    })
}

fn assistant_said(history: &[Message], needle: &str) -> bool {
    history.iter().any(|message| {
        let Message::Assistant { content, .. } = message else {
            return false;
        };
        content.iter().any(|block| match block {
            AssistantBlock::Text(text) => text.text.contains(needle),
            _ => false,
        })
    })
}

// ── 1. Durability without a clean exit ──────────────────────────────────────

#[tokio::test]
async fn a_completed_turn_survives_a_process_that_never_reaches_its_exit_path() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let host = build_host(data_dir.path()).await;
    let id = SessionId::from_string("crashed");
    host.create(&id, 25);

    let sessions = host.sessions();
    let status = host.status();
    let driver_id = id.clone();
    let driver = tokio::spawn(async move {
        run::drive_until(&sessions, &driver_id, Some(&status)).await;
    });

    host.submit("remember this").await;
    assert!(
        host.transcript.wait_for(ANSWER).await,
        "the scripted turn should have answered: {:#?}",
        host.transcript.lines()
    );
    assert!(
        poll_until(|| stored(data_dir.path(), &id)).await,
        "a completed turn must reach the store before the next one starts; \
         nothing was written to {}",
        data_dir.path().display()
    );

    // The ungraceful stop: the driver simply ceases to exist. No exit path, no
    // shutdown save, no `cleanup()` — the process equivalent of a closed
    // terminal, a dropped SSH session or an OOM kill.
    driver.abort();

    let history = recover_history(data_dir.path(), &id).await;
    assert!(
        assistant_said(&history, ANSWER),
        "the completed turn was lost with the process: {history:#?}"
    );
}

// ── 2. The session in force, not the session at launch ──────────────────────

#[tokio::test]
async fn shutdown_saves_the_session_in_force_not_the_one_the_host_launched_with() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let host = build_host(data_dir.path()).await;
    let sessions = host.sessions();

    // The launch identity, bound before the TUI runs — exactly what the
    // composition root used to hold on to for its one exit save.
    let launch = SessionId::from_string("launch");
    host.create(&launch, 25);
    let navigation = SessionNavigation::new(sessions.clone(), launch.clone());

    // A `/new` moves the session in force. The driver loop performs the
    // switch and tells navigation about it; from here on, "the session" means
    // `switched`.
    let switched = SessionId::from_string("switched");
    host.create(&switched, 25);
    navigation.set_current(switched.clone());
    assert_eq!(navigation.current(), switched);

    // One turn in the session the user is actually in, driven without the
    // per-turn save so that the shutdown save is the only thing that can
    // record it.
    let status = host.status();
    let turn = run::drive_turn_with_status(&sessions, &switched, Some(&status));
    tokio::pin!(turn);
    host.submit("work in the new session").await;
    let outcome = turn.await;
    assert_eq!(outcome.reason, run::TurnReason::Completed);

    // The shutdown path, given what the front end says is in force.
    let idle: tokio::task::JoinHandle<()> = tokio::spawn(std::future::pending());
    run::shutdown_driver(&sessions, &navigation.current(), &idle, GRACE)
        .await
        .expect("the shutdown save succeeds");

    assert!(
        stored(data_dir.path(), &switched),
        "shutdown did not record the session the user was in"
    );
    assert!(
        !stored(data_dir.path(), &launch),
        "shutdown recorded the launch identity instead; that is the bug — the \
         user's work is filed under a session they left"
    );
    let history = recover_history(data_dir.path(), &switched).await;
    assert!(
        assistant_said(&history, ANSWER),
        "the switched session's turn is missing from its own record: {history:#?}"
    );
}

// ── 3. A recoverable failure returns the user to input ──────────────────────

#[tokio::test]
async fn a_runaway_turn_returns_the_user_to_input_with_the_reason_on_screen() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    // A ceiling of two makes the runaway turn terminate in milliseconds; the
    // shipped executor uses 25 and reaches the same error.
    let host = build_host(data_dir.path()).await;
    let id = SessionId::from_string("runaway");
    host.create(&id, 2);

    let sessions = host.sessions();
    let status = host.status();
    let driver_id = id.clone();
    let driver = tokio::spawn(async move {
        run::drive_until(&sessions, &driver_id, Some(&status)).await;
    });

    host.runaway.store(true, Ordering::SeqCst);
    host.submit("go round in circles").await;
    assert!(
        host.transcript.wait_for("max iterations").await,
        "the iteration ceiling should have failed the turn: {:#?}",
        host.transcript.lines()
    );
    assert!(
        host.transcript.contains("send another message to retry"),
        "a recoverable failure must say so where the user is looking, not only \
         in the Logs panel: {:#?}",
        host.transcript.lines()
    );
    assert!(
        !driver.is_finished(),
        "an ordinary turn failure must not end the driver — the TUI would keep \
         accepting input nothing will ever consume"
    );

    // The proof that "still running" means something: the next prompt is
    // actually served.
    host.runaway.store(false, Ordering::SeqCst);
    host.submit("now behave").await;
    assert!(
        host.transcript.wait_for(ANSWER).await,
        "the driver survived the failure but served no further turn: {:#?}",
        host.transcript.lines()
    );

    driver.abort();
    let history = recover_history(data_dir.path(), &id).await;
    assert!(
        assistant_said(&history, ANSWER),
        "the recovered turn should also have been persisted: {history:#?}"
    );
}

// ── 4. An unrecoverable failure stops, rather than spinning ─────────────────

#[tokio::test]
async fn a_session_that_cannot_serve_a_turn_stops_the_driver_instead_of_spinning() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let host = build_host(data_dir.path()).await;
    let sessions = host.sessions();
    let status = host.status();

    // Never created. Every turn fails identically, before the graph ever parks
    // on input, so "keep going" would be a hot loop with nothing on screen.
    let missing = SessionId::from_string("never-created");
    let driver = tokio::spawn(async move {
        run::drive_until(&sessions, &missing, Some(&status)).await;
    });

    let stopped = tokio::time::timeout(PATIENCE, driver).await;
    assert!(
        stopped.is_ok(),
        "the driver span the unserviceable session instead of stopping"
    );
    assert!(
        host.transcript.contains("cannot serve another turn"),
        "stopping silently is the original bug in a different costume: {:#?}",
        host.transcript.lines()
    );
}

// ── The counterweight: give up rather than retry forever ────────────────────

#[tokio::test]
async fn a_session_that_fails_every_turn_gives_up_rather_than_retrying_forever() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let host = build_host(data_dir.path()).await;
    let id = SessionId::from_string("wedged");
    host.create(&id, 2);

    let sessions = host.sessions();
    let status = host.status();
    let driver_id = id.clone();
    let driver = tokio::spawn(async move {
        run::drive_until(&sessions, &driver_id, Some(&status)).await;
    });

    host.runaway.store(true, Ordering::SeqCst);
    // One more prompt than the budget allows, so the driver stops because it
    // gave up rather than because it ran out of input.
    //
    // Fed from its own task on purpose. The input channel holds exactly one
    // message, so a driver that has stopped consuming blocks the next
    // `send` forever — which is the user-facing shape of this whole bug, and
    // is why a stopped driver has to say so instead of just going quiet. A
    // test that submitted inline would hang on it rather than report it.
    let feeder = {
        let input = host.input.clone();
        tokio::spawn(async move {
            for turn in 0..=run::FailureBudget::DEFAULT_LIMIT {
                let prompt = IOMessage::from_external(
                    "resilience-proof",
                    IOContent::Text(format!("attempt {turn}")),
                );
                if input.send(prompt).await.is_err() {
                    return;
                }
            }
        })
    };

    let stopped = tokio::time::timeout(PATIENCE, driver).await;
    feeder.abort();
    assert!(
        stopped.is_ok(),
        "a driver that never gives up is a worse bug than one that gives up \
         too early: {:#?}",
        host.transcript.lines()
    );
    assert!(
        host.transcript.contains("Giving up after"),
        "giving up must be stated on screen: {:#?}",
        host.transcript.lines()
    );
}

// ── Cost: the per-turn save must not be felt ────────────────────────────────

#[tokio::test]
async fn per_turn_saving_stays_cheap_as_the_conversation_grows() {
    let data_dir = tempfile::tempdir().expect("a temp session store");
    let host = build_host(data_dir.path()).await;
    let id = SessionId::from_string("long");
    host.create(&id, 25);
    let sessions = host.sessions();
    let status = host.status();

    let mut worst = Duration::ZERO;
    for turn in 0..20u32 {
        let driving = run::drive_turn_with_status(&sessions, &id, Some(&status));
        tokio::pin!(driving);
        host.submit(&format!("turn {turn}")).await;
        let outcome = driving.await;
        assert_eq!(outcome.reason, run::TurnReason::Completed);

        let started = Instant::now();
        run::persist_turn(&sessions, &id)
            .await
            .expect("the per-turn save succeeds");
        worst = worst.max(started.elapsed());
    }

    // The save runs between turns, where the session's context lock is free
    // and the user is reading the answer that just finished. A generous
    // ceiling: the point is that it is not a second of latency, and that it
    // does not grow into one.
    assert!(
        worst < Duration::from_millis(250),
        "the per-turn save took {worst:?} at worst over 20 turns; that is \
         latency a user would feel, so the durability point needs to move"
    );
    eprintln!("per-turn save over 20 turns: worst {worst:?}");
}
