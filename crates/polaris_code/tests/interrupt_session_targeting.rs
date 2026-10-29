//! Regression coverage for interrupt targeting across terminal session switches.

use std::sync::{Arc, OnceLock};

use polaris::agent::Agent;
use polaris::graph::Graph;
use polaris::sessions::{AgentTypeId, InMemoryStore, SessionId, SessionsAPI};
use polaris::system::param::SystemContext;
use polaris::system::system;
use polaris_code::run;
use polaris_code::terminal::{CommandOutcome, CommandRegistry, SessionAction, SessionNavigation};
use polaris_tui::commands::CommandInvocation;
use tokio::sync::Notify;

static TURN_ENTERED: OnceLock<Notify> = OnceLock::new();

fn turn_entered() -> &'static Notify {
    TURN_ENTERED.get_or_init(Notify::new)
}

#[system]
async fn wait_for_interrupt() {
    turn_entered().notify_one();
    std::future::pending::<()>().await;
}

struct InterruptibleAgent;

impl Agent for InterruptibleAgent {
    fn name(&self) -> &'static str {
        "interrupt-session-targeting"
    }

    fn build(&self, graph: &mut Graph) {
        graph.add_system(wait_for_interrupt);
    }
}

fn create_session(sessions: &SessionsAPI, id: &SessionId) {
    sessions
        .create_session(
            SystemContext::new(),
            id,
            &AgentTypeId::from_name("interrupt-session-targeting"),
        )
        .unwrap();
}

#[tokio::test]
async fn interrupt_after_new_targets_only_the_current_session() {
    let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
    sessions.register_agent(InterruptibleAgent).unwrap();

    let original = SessionId::from_string("original");
    create_session(&sessions, &original);
    let navigation = SessionNavigation::new(sessions.clone(), original.clone());
    let commands = CommandRegistry::with_builtins();
    navigation.register(&commands).unwrap();

    let outcome = commands
        .dispatch(&CommandInvocation::new("new", "current"))
        .await;
    assert!(matches!(outcome, CommandOutcome::Control(_)));
    let target = match navigation.take_pending().expect("/new scheduled a switch") {
        SessionAction::New(target) => target,
        SessionAction::Resume(_) => panic!("/new must schedule a new session"),
    };
    sessions.save_session(&original).await.unwrap();
    create_session(&sessions, &target);
    navigation.set_current(target.clone());

    // This is the host callback shape: resolve the identity at interrupt time,
    // after any number of successful navigation transitions.
    let callback_navigation = Arc::new(OnceLock::<SessionNavigation>::new());
    assert!(callback_navigation.set(navigation.clone()).is_ok());
    let callback_sessions = sessions.clone();
    let interrupt = move || {
        let Some(navigation) = callback_navigation.get() else {
            return false;
        };
        run::request_interrupt(&callback_sessions, &navigation.current())
    };

    let turn_sessions = sessions.clone();
    let turn_id = target.clone();
    let turn = tokio::spawn(async move { turn_sessions.process_turn(&turn_id).await });
    turn_entered().notified().await;

    assert!(interrupt(), "the current session has a running turn");
    assert!(
        !run::request_interrupt(&sessions, &original),
        "the session the user left must not have been signalled"
    );
    turn.abort();

    // Keep this behavioral proof coupled to the real composition root. The
    // regression originated there: the old wiring captured `session_id` once,
    // while navigation changed the driver's current identity later.
    let composition = include_str!("../src/main.rs");
    assert!(
        composition.contains("callback_navigation.get()"),
        "the terminal interrupt callback must resolve SessionNavigation at interrupt time"
    );
    assert!(
        !composition.contains("let callback_id = session_id.clone();"),
        "the terminal interrupt callback must not capture the startup session id"
    );
}
