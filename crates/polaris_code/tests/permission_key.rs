//! Shift-Tab steps the permission mode, end to end.
//!
//! The pieces are all production code: the conversation panel as
//! `terminal::attach` decorates it, the `UserIO` the agent receives through,
//! the command dispatch behind it, and the real `/permission` command over the
//! real session settings. The only stand-in is the key press, delivered as a
//! [`TuiEvent`] instead of a crossterm loop. What is asserted is the posture
//! the session ends up in.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use polaris::plugins::UserIO;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version};
use polaris::system::server::Server;
use polaris_code::agents::PermissionMode;
use polaris_code::config::ModelCatalog;
use polaris_code::terminal::{self, CommandRegistry, SessionSettings};
use polaris_tui::commands::CommandSpec;
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::ConversationPanelPlugin;
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{EventOutcome, TuiEvent, TuiPanel, TuiRegistry};

const MODEL: &str = "scripted/claude-sonnet-4-6";

/// Longest the test waits for the panel or the dispatch to settle. Passing
/// runs return on the first match; the bound only keeps a broken run from
/// hanging the suite.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);
const SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Provides the [`TuiRegistry`] capability the way `TuiPlugin::build` does and
/// nothing else, so the contributed panel stays in the registry for the test.
struct HeadlessTuiPlugin;

impl Plugin for HeadlessTuiPlugin {
    const ID: &'static str = "test::headless_tui_permission_key";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }
}

fn shift_tab() -> TuiEvent {
    TuiEvent::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
}

/// The registered command called `name`, as the host will advertise it.
fn advertised(commands: &CommandRegistry, name: &str) -> CommandSpec {
    commands
        .specs()
        .into_iter()
        .find(|spec| spec.name == name)
        .unwrap_or_else(|| panic!("/{name} is registered"))
}

/// What the panel draws now, one line per terminal row.
fn drawn(panel: &mut dyn TuiPanel, terminal: &mut Terminal<TestBackend>) -> String {
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content
        .chunks(buffer.area.width as usize)
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait until the host has advertised `command`, so that a key pressed next is
/// judged by a panel that knows the host's commands. Once the host has
/// advertised any, a typed `/` opens a popup that shows each command's
/// description, and that popup is the evidence; the typed `/` is deleted again
/// before this returns.
async fn wait_until_advertised(panel: &mut dyn TuiPanel, command: &CommandSpec) {
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    loop {
        panel.tick();
        panel.handle_event(&TuiEvent::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        )));
        let screen = drawn(panel, &mut terminal);
        panel.handle_event(&TuiEvent::Key(KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        if screen.contains(&command.description) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the host never advertised /{}:\n{screen}",
            command.name
        );
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    }
}

async fn settles_on(settings: &SessionSettings, expected: PermissionMode) -> bool {
    let deadline = tokio::time::Instant::now() + SETTLE_DEADLINE;
    while tokio::time::Instant::now() < deadline {
        if settings.snapshot().permission_mode == expected {
            return true;
        }
        tokio::time::sleep(SETTLE_POLL_INTERVAL).await;
    }
    false
}

/// A host over `commands`, wired as the interactive front end wires its panel,
/// with `scenario` run against that panel while the agent's side waits for a
/// prompt. Waiting is what advertises the host's commands and runs the ones
/// the panel sends, so the scenario only sees what a user would.
async fn with_host<F, Fut>(commands: Arc<CommandRegistry>, scenario: F)
where
    F: FnOnce(Box<dyn TuiPanel>) -> Fut,
    Fut: Future<Output = ()>,
{
    let working_dir = std::env::current_dir().unwrap();
    let mut server = Server::new();
    server.add_plugins(HeadlessTuiPlugin);
    let (conversation, _output) =
        terminal::attach_with_output(ConversationPanelPlugin::default(), commands, &working_dir);
    server.add_plugins(conversation);
    server.finish().await.unwrap();

    let panel = server
        .api::<TuiRegistry>()
        .unwrap()
        .drain()
        .into_iter()
        .find(|panel| panel.id() == "conversation")
        .expect("the conversation panel contributed itself");

    let context = server.create_context();
    let io = context
        .get_resource::<UserIO>()
        .expect("the panel registered a UserIO");
    tokio::select! {
        _ = io.receive() => panic!("the agent received a prompt nobody typed"),
        () = scenario(panel) => {}
    }
}

#[tokio::test]
async fn shift_tab_steps_the_session_through_the_permission_modes() {
    let settings =
        SessionSettings::new(MODEL, PermissionMode::Confirm, &ModelCatalog::pinned(MODEL));
    let commands = Arc::new(CommandRegistry::with_builtins());
    settings.register(&commands).expect("commands register");
    let permission = advertised(&commands, "permission");

    with_host(commands, |mut panel| async move {
        wait_until_advertised(panel.as_mut(), &permission).await;
        for (step, expected) in [
            ("first", PermissionMode::AutoEdit),
            ("second", PermissionMode::Auto),
            ("third", PermissionMode::Plan),
            ("fourth", PermissionMode::Confirm),
        ] {
            assert_eq!(panel.handle_event(&shift_tab()), EventOutcome::Consumed);
            assert!(
                settles_on(&settings, expected.clone()).await,
                "the {step} press must reach {}",
                expected.label()
            );
        }
    })
    .await;
}

#[tokio::test]
async fn shift_tab_is_left_to_the_shell_when_the_host_has_no_permission_command() {
    // No `/permission` registered, so the host never advertises one.
    let commands = Arc::new(CommandRegistry::with_builtins());
    let help = advertised(&commands, "help");

    with_host(commands, |mut panel| async move {
        // `Continue` is also the answer before the host has advertised anything,
        // so first wait until another of its commands is visible.
        wait_until_advertised(panel.as_mut(), &help).await;
        assert_eq!(
            panel.handle_event(&shift_tab()),
            EventOutcome::Continue,
            "without `/permission` the key must still reach the shell"
        );
    })
    .await;
}
