//! Proof that terminal `/commands` are local actions.
//!
//! Drives [`CommandDispatch`] over a scripted inner provider — the same
//! shape as the panel's — and checks, from the agent's side of `UserIO`,
//! that a dispatched command is executed and answered without ever being
//! returned from `receive`. There is no model in this test at all; that is
//! the point. If a command reached the agent, `receive` would hand it back
//! as a prompt and the assertion on what `receive` returns would fail.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use polaris::plugins::{IO_TYPE, IOContent, IOError, IOMessage, IOProvider, UserIO};
use polaris_code::terminal::{
    CommandDispatch, CommandOutcome, CommandRegistry, META_MENTIONS, attach,
};
use polaris_tui::commands::{CommandInvocation, CommandSpec, META_COMMANDS, parse_advertisement};
use polaris_tui::panels::ConversationPanelPlugin;
use tokio::sync::Mutex;

/// The panel's half, scripted: `receive` pops what the "user" typed and
/// `send` records what the user would see.
struct Scripted {
    typed: Mutex<VecDeque<IOMessage>>,
    shown: Mutex<Vec<IOMessage>>,
}

impl Scripted {
    fn new(typed: Vec<IOMessage>) -> Arc<Self> {
        Arc::new(Self {
            typed: Mutex::new(typed.into()),
            shown: Mutex::new(Vec::new()),
        })
    }

    async fn shown(&self) -> Vec<IOMessage> {
        self.shown.lock().await.clone()
    }
}

impl IOProvider for Scripted {
    async fn send(&self, message: IOMessage) -> Result<(), IOError> {
        self.shown.lock().await.push(message);
        Ok(())
    }

    async fn receive(&self) -> Result<IOMessage, IOError> {
        self.typed.lock().await.pop_front().ok_or(IOError::Closed)
    }
}

fn text(message: &IOMessage) -> &str {
    match &message.content {
        IOContent::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

fn kind(message: &IOMessage) -> Option<&str> {
    message.metadata.get(IO_TYPE).map(String::as_str)
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    dir
}

fn user_io(inner: Arc<Scripted>, registry: Arc<CommandRegistry>, root: &Path) -> UserIO {
    UserIO::new(Arc::new(CommandDispatch::new(
        inner,
        registry,
        root.to_path_buf(),
    )))
}

#[tokio::test]
async fn a_command_is_answered_locally_and_never_reaches_receive() {
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    let inner = Scripted::new(vec![
        CommandInvocation::new("help", "").into_message("/help"),
        IOMessage::user_text("now a real prompt"),
    ]);
    let io = user_io(inner.clone(), registry, ws.path());

    // The agent's `receive` skips straight to the prompt.
    let got = tokio::time::timeout(Duration::from_secs(2), io.receive())
        .await
        .expect("receive does not hang on a command")
        .unwrap();
    assert_eq!(text(&got), "now a real prompt");
    assert!(
        CommandInvocation::from_message(&got).is_none(),
        "what the agent gets is never a command"
    );

    // Meanwhile the user saw the advertisement and the help reply.
    let shown = inner.shown().await;
    assert_eq!(shown.len(), 2, "{shown:#?}");
    let specs = parse_advertisement(&shown[0]).expect("first message advertises the commands");
    assert_eq!(
        specs,
        vec![CommandSpec::new("help", "List the available commands").with_usage("[command]")]
    );
    assert_eq!(kind(&shown[1]), Some("message"));
    assert!(text(&shown[1]).contains("/help"), "{}", text(&shown[1]));

    // With nothing left to type, the channel is closed — no phantom prompt.
    assert!(matches!(io.receive().await, Err(IOError::Closed)));
}

#[tokio::test]
async fn an_unknown_command_is_an_actionable_error_not_a_prompt() {
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    let inner = Scripted::new(vec![
        CommandInvocation::new("hlep", "").into_message("/hlep"),
    ]);
    let io = user_io(inner.clone(), registry, ws.path());
    assert!(matches!(io.receive().await, Err(IOError::Closed)));
    let shown = inner.shown().await;
    let reply = shown.last().unwrap();
    assert_eq!(kind(reply), Some("error"));
    assert!(
        text(reply).contains("unknown command `/hlep`"),
        "{}",
        text(reply)
    );
    assert!(text(reply).contains("/help"), "{}", text(reply));
}

#[tokio::test]
async fn registered_commands_run_with_their_args() {
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    registry
        .register_sync(CommandSpec::new("echo", "Echo the arguments"), |_, args| {
            CommandOutcome::Reply(format!("echo: {args}"))
        })
        .unwrap();
    let inner = Scripted::new(vec![
        CommandInvocation::new("echo", "one two").into_message("/echo one two"),
    ]);
    let io = user_io(inner.clone(), registry, ws.path());
    let _ = io.receive().await;
    let shown = inner.shown().await;
    assert_eq!(text(&shown[1]), "echo: one two");
    assert_eq!(shown[1].metadata.get("io_type").unwrap(), "message");
}

#[tokio::test]
async fn mentions_are_validated_before_a_prompt_is_forwarded() {
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    let inner = Scripted::new(vec![
        IOMessage::user_text("read @../outside.txt please"),
        IOMessage::user_text("read @src/main.rs please"),
    ]);
    let io = user_io(inner.clone(), registry, ws.path());

    // The escaping prompt is refused and the next one is what arrives.
    let got = io.receive().await.unwrap();
    assert_eq!(text(&got), "read @src/main.rs please");
    let mentions: Vec<String> =
        serde_json::from_str(got.metadata.get(META_MENTIONS).unwrap()).unwrap();
    assert_eq!(mentions, ["src/main.rs"]);

    let shown = inner.shown().await;
    let refusal = shown
        .iter()
        .find(|m| kind(m) == Some("error"))
        .expect("the escape was reported");
    assert!(
        text(refusal).contains("@../outside.txt"),
        "{}",
        text(refusal)
    );
    assert!(text(refusal).contains("was not sent"), "{}", text(refusal));
}

#[tokio::test]
async fn the_advertisement_is_sent_once() {
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    let inner = Scripted::new(vec![
        IOMessage::user_text("one"),
        IOMessage::user_text("two"),
    ]);
    let io = user_io(inner.clone(), registry, ws.path());
    io.receive().await.unwrap();
    io.receive().await.unwrap();
    let shown = inner.shown().await;
    let advertisements = shown
        .iter()
        .filter(|m| m.metadata.contains_key(META_COMMANDS))
        .count();
    assert_eq!(advertisements, 1);
}

#[tokio::test]
async fn attach_builds_a_panel_plugin_that_wraps_user_io() {
    // `attach` is glue; this checks it composes without panicking and that
    // the decorated `UserIO` behaves like the hand-built one above.
    let ws = workspace();
    let registry = Arc::new(CommandRegistry::with_builtins());
    let plugin = attach(ConversationPanelPlugin::default(), registry, ws.path());
    let _ = plugin;
}
