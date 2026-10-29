//! End-to-end proof that a file attached at the composer reaches the model as
//! a real image block — and that everything the host refuses is refused before
//! anything is sent.
//!
//! The edges are the only fakes. A capturing [`LlmProvider`] stands in for
//! `AnthropicPlugin`, so there is no key and no network, and the panel is
//! driven with [`TuiEvent`]s instead of a crossterm loop. Everything between
//! is production code: keystrokes go through the real composer, the chip comes
//! from the real [`WorkspaceAttachments`] policy, the submission crosses the
//! real bounded channel into `CommandDispatch`, and `receive_user_input`
//! redeems the handle out of the real [`AttachmentStore`].
//!
//! What is asserted at the far end is the request the provider was handed —
//! not a summary of it — because the one thing that must never happen is an
//! attachment arriving as a sentence about an attachment.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt as _;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, DocumentSource, GenerationError,
    ImageMediaType, LlmProvider, LlmRequest, LlmResponse, LlmStream, Message, StopReason,
    StreamEvent, Usage, UserBlock,
};
use polaris::models::{ModelRegistry, ModelsPlugin};
use polaris::plugins::{PersistenceAPI, PersistencePlugin};
use polaris::sessions::{AgentTypeId, FileStore, SessionId, SessionsAPI, SessionsPlugin};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::ToolsPlugin;
use polaris_code::agents::{AgentConfig, CodeAgent, CodeAgentPlugin, PermissionMode};
use polaris_code::config::ModelCatalog;
use polaris_code::run;
use polaris_code::terminal::{
    self, ATTACHMENT_BYTES_MAX, AttachmentStore, CommandRegistry, SessionSettings, TurnSteering,
    WorkspaceAttachments,
};
use polaris_code::usage::TokenMeter;
use polaris_ontrace::oxigraph::OxigraphPlugin;
use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiEvent, TuiPanel, TuiRegistry};

use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A vision-capable model id, in the registry's `provider/model` form.
const VISION_MODEL: &str = "scripted/claude-sonnet-4-6";
/// A model this host knows takes text only.
const TEXT_ONLY_MODEL: &str = "scripted/claude-3-5-haiku-latest";

/// Maximum time to observe an asynchronous panel or turn condition settle.
///
/// Passing paths return on the first matching frame. The bound exists only so
/// genuinely broken behaviour fails instead of hanging the suite indefinitely;
/// scheduler load can delay an observation within the bound, but cannot make an
/// unmet condition pass.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);
/// Polling cadence while waiting for an asynchronous condition to settle.
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(20);

const USAGE: Usage = Usage {
    input_tokens: Some(100),
    output_tokens: Some(10),
    total_tokens: Some(110),
    cache_read_tokens: None,
    cache_creation_tokens: None,
    reasoning_output_tokens: None,
};

/// A minimal but genuine PNG header, enough for the host's magic-byte sniff.
const PNG: &[u8] = &[
    0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89,
];

// ─────────────────────────────────────────────────────────────────────────────
// The capturing model
// ─────────────────────────────────────────────────────────────────────────────

/// Answers every turn identically and keeps every request it was handed.
#[derive(Default)]
struct CapturingProvider {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl LlmProvider for CapturingProvider {
    fn name(&self) -> &'static str {
        "scripted"
    }

    async fn generate(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.seen.lock().unwrap().push(request.messages.clone());
        Ok(LlmResponse {
            content: vec![AssistantBlock::text("seen")],
            usage: USAGE,
            stop_reason: StopReason::EndTurn,
            id: None,
            model: None,
        })
    }

    async fn stream(
        &self,
        _model: &str,
        request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        self.seen.lock().unwrap().push(request.messages.clone());
        let events = vec![
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                block: ContentBlockStartData::Text,
            }),
            Ok(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: ContentBlockDelta::Text("seen".to_string()),
            }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageStop {
                stop_reason: StopReason::EndTurn,
                usage: USAGE,
                id: None,
                model: None,
            }),
        ];
        Ok(futures_util::stream::iter(events).boxed())
    }
}

struct CapturingModelPlugin {
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Plugin for CapturingModelPlugin {
    const ID: &'static str = "test::attachment_model";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ModelsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        server
            .get_resource_mut::<ModelRegistry>()
            .expect("ModelsPlugin must build before the capturing model")
            .register_llm_provider(CapturingProvider {
                seen: self.seen.clone(),
            });
    }
}

/// Provides the [`TuiRegistry`] capability the way `TuiPlugin::build` does and
/// nothing else, so the contributed panel stays in the registry for the test.
struct HeadlessTuiPlugin;

impl Plugin for HeadlessTuiPlugin {
    const ID: &'static str = "test::headless_tui_attachments";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// The host
// ─────────────────────────────────────────────────────────────────────────────

struct Host {
    panel: Box<dyn TuiPanel>,
    sessions: SessionsAPI,
    session_id: SessionId,
    store: AttachmentStore,
    seen: Arc<Mutex<Vec<Vec<Message>>>>,
    terminal: Terminal<TestBackend>,
    steering: TurnSteering,
}

impl Host {
    /// Boot a host over `working_dir`, wired as `main.rs` wires the
    /// interactive front end.
    ///
    /// `steer` selects which of the two submission paths the test drives:
    /// `true` is the interactive one — a `TurnInputHandle` and the steering
    /// queue — and `false` is the bare `UserIO` channel a consumer without a
    /// queue uses. Both must carry an attachment.
    async fn boot(working_dir: &Path, model: &str, steer: bool) -> Self {
        let working_dir = working_dir.canonicalize().unwrap();
        let seen: Arc<Mutex<Vec<Vec<Message>>>> = Arc::default();
        let config = AgentConfig::new(model, working_dir.clone());
        let commands = Arc::new(CommandRegistry::with_builtins());
        let store = AttachmentStore::new();
        let attachment_settings = Arc::new(OnceLock::<SessionSettings>::new());
        let steering = if steer {
            TurnSteering::enabled()
        } else {
            TurnSteering::default()
        };

        let mut server = Server::new();
        server
            .add_plugins(ModelsPlugin)
            .add_plugins(CapturingModelPlugin { seen: seen.clone() })
            .add_plugins(ToolsPlugin)
            .add_plugins(OxigraphPlugin::in_memory())
            .add_plugins(PersistencePlugin)
            .add_plugins(SessionsPlugin::new(Arc::new(FileStore::new(
                working_dir.join(".polaris/sessions"),
            ))))
            .add_plugins(CodeAgentPlugin::new())
            .add_plugins(HeadlessTuiPlugin);

        let mut panel_plugin = ConversationPanelPlugin::default().with_attachment_source(Arc::new(
            WorkspaceAttachments::new(&working_dir, store.clone(), model)
                .with_settings(attachment_settings.clone())
                // The platform helper is irrelevant to what is being proven and
                // would make the suite depend on what happens to be installed.
                .without_clipboard(),
        ));
        if steer {
            panel_plugin = panel_plugin.with_turn_input_handle(Arc::new(steering.clone()));
        }
        let (conversation, _output) =
            terminal::attach_with_output(panel_plugin, commands.clone(), &working_dir);
        server.add_plugins(conversation);
        server.insert_global(TokenMeter::new());
        server.finish().await.unwrap();

        let panel = server
            .api::<TuiRegistry>()
            .unwrap()
            .drain()
            .into_iter()
            .find(|panel| panel.id() == "conversation")
            .expect("the conversation panel contributed itself");

        let settings = {
            let models = server.get_global::<ModelRegistry>().unwrap();
            let catalog = ModelCatalog::new(model, [VISION_MODEL, TEXT_ONLY_MODEL])
                .restricted_to_providers(&models.llm_provider_names());
            SessionSettings::new(model, PermissionMode::Confirm, &catalog)
        };
        let _ = attachment_settings.set(settings.clone());

        let persistence = server.api::<PersistenceAPI>().unwrap();
        let sessions = server.api::<SessionsAPI>().unwrap().clone();
        sessions.set_serializers(persistence.serializers());
        sessions.register_agent(CodeAgent).unwrap();

        let session_id = SessionId::from_string("attachments");
        sessions
            .create_session_with_executor(
                server.create_context(),
                &session_id,
                &AgentTypeId::from_name(CodeAgent::NAME),
                polaris::graph::GraphExecutor::new().with_default_max_iterations(8),
                {
                    let config = config.clone();
                    let settings = settings.clone();
                    let steering = steering.clone();
                    let store = store.clone();
                    move |ctx| {
                        ctx.insert(config.clone());
                        ctx.insert(settings.clone());
                        ctx.insert(steering.clone());
                        ctx.insert(store.clone());
                    }
                },
            )
            .unwrap();

        Self {
            panel,
            sessions,
            session_id,
            store,
            seen,
            terminal: Terminal::new(TestBackend::new(96, 24)).unwrap(),
            steering,
        }
    }

    fn key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        self.panel
            .handle_event(&TuiEvent::Key(KeyEvent::new(code, modifiers)));
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.key(KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// Tick the panel and draw its current state without implying it is settled.
    fn draw_frame(&mut self) -> String {
        self.panel.tick();
        let panel = &mut self.panel;
        self.terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        let buffer = self.terminal.backend().buffer();
        buffer
            .content
            .chunks(buffer.area.width as usize)
            .map(|cells| {
                cells
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Return the first frame satisfying `settled`, or fail with the last frame.
    ///
    /// This is the assertion-facing frame helper: callers cannot receive an
    /// asynchronous snapshot that did not meet the condition they named. It
    /// returns immediately on success, while the deadline bounds only how long
    /// the test waits to prove the condition is genuinely absent.
    async fn frame_when(&mut self, description: &str, settled: impl Fn(&str) -> bool) -> String {
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            let screen = self.draw_frame();
            if settled(&screen) {
                return screen;
            }
            assert!(
                tokio::time::Instant::now() < expiry,
                "{description}: {screen}"
            );
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
    }

    /// Type a draft, wait for the host's answer about it, and submit.
    async fn compose_and_send(&mut self, draft: &str) -> String {
        self.type_text(draft);
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            // Submission itself is the exhaustive condition for both visible
            // outcomes (a chip or refusal) and silent NotAttachable outcomes.
            // The production panel refuses Enter while a resolve is in flight,
            // so retrying it observes the exact boundary the user depends on.
            let settled_screen = self.draw_frame();
            self.key(KeyCode::Enter, KeyModifiers::NONE);
            let after_submit = self.draw_frame();
            if !after_submit.contains("still checking an attachment") {
                return settled_screen;
            }
            assert!(
                tokio::time::Instant::now() < expiry,
                "attachment resolution did not permit submission: {after_submit}"
            );
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
    }

    /// Run one turn to completion.
    async fn turn(&mut self) {
        let sessions = self.sessions.clone();
        let id = self.session_id.clone();
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        let driver = tokio::spawn(async move {
            run::drive_turn(&sessions, &id).await;
            flag.store(true, Ordering::SeqCst);
        });
        let expiry = tokio::time::Instant::now() + SETTLE_DEADLINE;
        loop {
            if done.load(Ordering::SeqCst) {
                driver
                    .await
                    .expect("the completed turn driver did not panic");
                return;
            }
            let screen = self.draw_frame();
            if screen.contains("was not sent") {
                // The prompt gate deliberately loops for another prompt. The
                // test has observed its terminal condition, so stop the parked
                // driver instead of mistaking the intentional wait for a turn
                // that failed to settle.
                driver.abort();
                let _ = driver.await;
                return;
            }
            if tokio::time::Instant::now() >= expiry {
                driver.abort();
                let _ = driver.await;
                panic!("turn did not finish before the bounded deadline: {screen}");
            }
            tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
        }
    }

    /// The user message the model was handed on the first request of the turn.
    fn first_user_message(&self) -> Message {
        let seen = self.seen.lock().unwrap();
        let request = seen.first().expect("the model was called at least once");
        request
            .iter()
            .find(|message| matches!(message, Message::User { .. }))
            .expect("a turn starts from a user message")
            .clone()
    }

    fn model_was_called(&self) -> bool {
        !self.seen.lock().unwrap().is_empty()
    }
}

/// A workspace with one real image, one ordinary file, and one liar.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("design")).unwrap();
    std::fs::write(dir.path().join("design/diagram.png"), PNG).unwrap();
    std::fs::write(dir.path().join("notes.txt"), b"plain text").unwrap();
    std::fs::write(dir.path().join("fake.png"), b"PK\x03\x04 not an image").unwrap();
    dir
}

/// The image blocks of a user message, with their decoded media types.
fn image_blocks(message: &Message) -> Vec<(ImageMediaType, String)> {
    let Message::User { content } = message else {
        panic!("expected a user message");
    };
    content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Image(image) => {
                let DocumentSource::Base64(data) = &image.data;
                Some((image.media_type.clone(), data.clone()))
            }
            _ => None,
        })
        .collect()
}

fn text_blocks(message: &Message) -> Vec<String> {
    let Message::User { content } = message else {
        panic!("expected a user message");
    };
    content
        .iter()
        .filter_map(|block| match block {
            UserBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .collect()
}

/// The base64 the model should have been handed for [`PNG`].
fn expected_png_base64() -> String {
    // Computed here rather than pasted, so the assertion checks the pipeline
    // and not a transcription.
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in PNG.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// The proofs
// ─────────────────────────────────────────────────────────────────────────────

/// The whole point: a mentioned image reaches the provider as an image.
#[tokio::test]
async fn an_attached_image_reaches_the_model_as_an_image_block() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;

    let composing = host.compose_and_send("explain @design/diagram.png").await;
    assert!(
        composing.contains("attached") && composing.contains("diagram.png"),
        "the chip is on screen before the prompt goes: {composing}"
    );
    assert_eq!(host.store.len(), 1, "and its bytes are parked for the turn");

    host.turn().await;

    let message = host.first_user_message();
    let images = image_blocks(&message);
    assert_eq!(images.len(), 1, "exactly one image reached the model");
    assert!(
        matches!(images[0].0, ImageMediaType::PNG),
        "typed from the file's magic bytes: {:?}",
        images[0].0
    );
    assert_eq!(
        images[0].1,
        expected_png_base64(),
        "the bytes are the file's, base64 of the real thing"
    );
    assert_eq!(
        text_blocks(&message),
        ["explain @design/diagram.png"],
        "and the prompt is the user's own words, not a description of the image"
    );
    assert!(
        host.store.is_empty(),
        "the handle was redeemed, so a replayed prompt cannot double-send it"
    );
}

/// The same has to hold on the plain `UserIO` channel, which a consumer with
/// no queue contract uses.
#[tokio::test]
async fn the_channel_path_carries_an_attachment_too() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), VISION_MODEL, false).await;

    host.compose_and_send("what is @design/diagram.png").await;
    host.turn().await;

    let message = host.first_user_message();
    assert_eq!(image_blocks(&message).len(), 1);
    assert_eq!(text_blocks(&message), ["what is @design/diagram.png"]);
}

/// Removing the chip removes the attachment, and nothing about the turn
/// remembers it.
#[tokio::test]
async fn removing_the_chip_sends_the_prompt_without_the_image() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;

    host.type_text("explain @design/diagram.png please");
    let attached = host
        .frame_when("the attachment never appeared", |screen| {
            screen.contains("attached")
        })
        .await;
    assert!(attached.contains("attached"), "{attached}");

    host.key(KeyCode::Char('x'), KeyModifiers::CONTROL);
    // Instant on purpose: Ctrl-X removes the chip synchronously, so this is a
    // fact about the present event handler, not an asynchronous settled result.
    let removed = host.draw_frame();
    assert!(
        !removed.contains("attached"),
        "the chip row is gone: {removed}"
    );

    host.key(KeyCode::Enter, KeyModifiers::NONE);
    host.draw_frame();
    host.turn().await;

    let message = host.first_user_message();
    assert!(
        image_blocks(&message).is_empty(),
        "no image went with a prompt whose chip was removed"
    );
    assert_eq!(
        text_blocks(&message),
        ["explain please"],
        "and the mention left the draft with the chip"
    );
}

/// A model that cannot read images refuses at the composer, where the user can
/// still do something about it.
#[tokio::test]
async fn a_text_only_model_refuses_before_the_turn_starts() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), TEXT_ONLY_MODEL, true).await;

    let screen = host.compose_and_send("explain @design/diagram.png").await;
    assert!(
        screen.contains("does not read images"),
        "the refusal names the reason: {screen}"
    );
    assert!(screen.contains("/model"), "and the way out: {screen}");
    assert!(
        !screen.contains("attached "),
        "no chip was created: {screen}"
    );
    assert!(host.store.is_empty());

    host.turn().await;
    let message = host.first_user_message();
    assert!(
        image_blocks(&message).is_empty(),
        "and the prompt still went, carrying only its text"
    );
    assert_eq!(text_blocks(&message), ["explain @design/diagram.png"]);
}

/// A file that is not an image, and a file that only claims to be one, both
/// stay ordinary mentions — silently, because that is what mentioning a source
/// file has always meant.
#[tokio::test]
async fn an_ordinary_file_and_a_misnamed_one_stay_text() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;

    let screen = host.compose_and_send("read @notes.txt and @fake.png").await;
    assert!(!screen.contains("attached "), "no chip: {screen}");
    assert!(
        !screen.to_lowercase().contains("cannot"),
        "and no complaint about an ordinary mention: {screen}"
    );

    host.turn().await;
    let message = host.first_user_message();
    assert!(image_blocks(&message).is_empty());
    assert_eq!(text_blocks(&message), ["read @notes.txt and @fake.png"]);
}

/// An oversized image is refused with both numbers, before it is read.
#[tokio::test]
async fn an_oversized_image_is_refused_with_its_size() {
    let dir = workspace();
    let mut big = PNG.to_vec();
    big.resize(ATTACHMENT_BYTES_MAX as usize + 1, 0);
    std::fs::write(dir.path().join("huge.png"), &big).unwrap();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;

    host.type_text("look at @huge.png");
    let screen = host
        .frame_when("the size refusal never appeared", |screen| {
            screen.contains("capped at")
        })
        .await;
    assert!(
        screen.contains("capped at"),
        "the cap is quoted so the user can compare: {screen}"
    );
    assert!(!screen.contains("attached "), "{screen}");
    assert!(
        host.store.is_empty(),
        "nothing that large was ever held in memory for a turn"
    );
    assert_eq!(
        host.panel.id(),
        "conversation",
        "the panel is still the panel — a refusal is not a crash"
    );
}

/// The attachment gate refuses a path outside the workspace, on either
/// submission path: no image leaves the working directory.
#[tokio::test]
async fn a_path_escape_never_becomes_an_attachment() {
    let dir = workspace();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("leak.png"), PNG).unwrap();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;

    host.type_text("show me @../leak.png");
    let screen = host
        .frame_when("the path refusal never appeared", |screen| {
            screen.contains("outside the working directory")
        })
        .await;
    assert!(
        screen.contains("outside the working directory"),
        "the attachment gate refuses it, in words that say why: {screen}"
    );
    assert!(!screen.contains("attached "), "and grows no chip: {screen}");
    assert!(
        host.store.is_empty(),
        "nothing outside the workspace was ever read into the store"
    );

    host.key(KeyCode::Enter, KeyModifiers::NONE);
    host.draw_frame();
    host.turn().await;
    // The prompt gate reaches this path too now, so the submission is refused
    // at the composer and the turn never starts — there is no request left to
    // inspect for an image block, which is strictly stronger than the
    // image-free request this used to assert on. Everything the *attachment*
    // gate owns is unchanged and asserted above: no chip, an empty store,
    // nothing read.
    assert!(
        !host.model_was_called(),
        "and nothing about a path outside the workspace reached a model at all"
    );
}

/// The prompt gate refuses the prompt itself on the `UserIO` channel, in
/// `CommandDispatch` — the decorator that loops for another prompt rather than
/// answering with a model call, so the turn is abandoned.
///
/// A host with a `TurnInputHandle` submits into its own queue and never
/// crosses this decorator, so that path is gated at the queue's own ingress
/// instead and proven in `tests/mention_containment.rs`. Both refuse; they
/// differ only in where the user hears about it.
#[tokio::test]
async fn a_path_escape_is_refused_by_the_prompt_gate_on_the_channel() {
    let dir = workspace();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("leak.png"), PNG).unwrap();
    let mut host = Host::boot(dir.path(), VISION_MODEL, false).await;

    host.type_text("show me @../leak.png");
    host.frame_when("the attachment gate never refused the escape", |screen| {
        screen.contains("outside the working directory")
    })
    .await;
    host.key(KeyCode::Enter, KeyModifiers::NONE);
    host.draw_frame();
    // The gate is in the decorator around `receive`, so it fires when the
    // agent asks for the prompt — not when the composer hands it over. The
    // turn then waits for a prompt that will never come and is abandoned,
    // which is exactly the shape a refusal has: nothing was consumed.
    host.turn().await;
    let after = host
        .frame_when("the prompt gate never reported its refusal", |screen| {
            screen.contains("was not sent")
        })
        .await;
    assert!(
        after.contains("was not sent"),
        "the prompt gate refuses the prompt as it always did: {after}"
    );
    assert!(
        !host.model_was_called(),
        "so the model was never asked about a path outside the workspace"
    );
}

/// The attachment is part of the session, not part of one request: it is
/// persisted with the canonical history and is still there after a resume.
#[tokio::test]
async fn an_attachment_persists_into_the_resumed_session() {
    let dir = workspace();
    let mut host = Host::boot(dir.path(), VISION_MODEL, true).await;
    host.compose_and_send("explain @design/diagram.png").await;
    host.turn().await;

    // The canonical history the session saved, read back the way the terminal's
    // replay reads it.
    let history = Arc::new(terminal::history::CanonicalHistory::default());
    history
        .refresh(&host.sessions, &host.session_id)
        .await
        .expect("the session has canonical history");
    let replayed = history.replay().expect("history replays");
    let rendered: Vec<String> = replayed
        .iter()
        .map(|message| match &message.content {
            polaris::plugins::IOContent::Text(text) => text.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert!(
        rendered.iter().any(|line| line.contains("[image]")),
        "replay shows that an image was part of the turn: {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|line| line.contains("explain @design/diagram.png")),
        "alongside the prompt it went with: {rendered:?}"
    );

    // And the stored context itself still holds the image block, not a summary
    // of one — a second turn re-sends it to the model.
    host.steering
        .submit_prompt_for_test("and again")
        .expect("queue a follow-up");
    host.turn().await;
    let requests = host.seen.lock().unwrap().clone();
    let last = requests.last().expect("a second turn ran");
    let images: usize = last
        .iter()
        .filter(|message| matches!(message, Message::User { .. }))
        .map(|message| image_blocks(message).len())
        .sum();
    assert_eq!(
        images, 1,
        "the image is still in the window on the next turn, once, out of persisted history"
    );
}

/// Convenience for the persistence proof: queue a prompt without going through
/// the panel, so the assertion is about the stored history rather than the
/// composer.
trait QueueForTest {
    fn submit_prompt_for_test(&self, text: &str) -> Result<(), String>;
}

impl QueueForTest for TurnSteering {
    fn submit_prompt_for_test(&self, text: &str) -> Result<(), String> {
        use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode};
        self.submit(text.to_string(), TurnInputMode::Queue)
            .map(|_| ())
    }
}
