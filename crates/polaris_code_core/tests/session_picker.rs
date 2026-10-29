//! `/sessions` is a picker: a user resumes yesterday's work by choosing a row,
//! not by reading an identifier off the screen and retyping it.
//!
//! The choice still travels the one scheduling path a switch has always taken
//! ([`SessionNavigation::take_pending`]), so what a selection buys is a way to
//! *say* which session, never a second way to move the active one. The
//! turn-boundary half of that proof lives in `session_switch_turn_boundary.rs`.

use std::sync::Arc;

use polaris::models::llm::Message;
use polaris::plugins::Storable;
use polaris::sessions::{
    InMemoryStore, ResourceEntry, SessionData, SessionId, SessionStore, SessionsAPI,
};
use polaris_code_core::agents::ContextManager;
use polaris_code_core::terminal::{
    CommandOutcome, CommandRegistry, SessionAction, SessionNavigation,
};
use polaris_tui::commands::CommandInvocation;

/// A saved session: `turns` turns, created at `started`, opened by `opening`.
async fn save(
    store: &Arc<dyn SessionStore>,
    id: &str,
    turns: u32,
    started: &str,
    opening: Option<&str>,
) {
    let resources = opening
        .map(|text| {
            let mut context = ContextManager::default();
            context.messages.push(Message::user(text));
            vec![ResourceEntry {
                plugin_id: "polaris_code::agents::code".into(),
                storage_key: ContextManager::storage_key().into(),
                version: ContextManager::schema_version().into(),
                data: serde_json::to_value(&context).unwrap(),
            }]
        })
        .unwrap_or_default();
    store
        .save(
            &SessionId::from_string(id),
            &SessionData::new("CodeAgent", turns, started, resources),
        )
        .await
        .unwrap();
}

/// Navigation over `store`, current session `current`, commands registered.
fn navigate(store: &Arc<dyn SessionStore>, current: &str) -> (SessionNavigation, CommandRegistry) {
    let navigation = SessionNavigation::new(
        SessionsAPI::new(store.clone()),
        SessionId::from_string(current),
    )
    .with_store(store.clone());
    let registry = CommandRegistry::with_builtins();
    navigation.register(&registry).unwrap();
    (navigation, registry)
}

/// The numbered rows of a rendered picker: `1. ...`, `2. ...`, and so on.
fn rows(text: &str) -> Vec<&str> {
    text.lines()
        .filter(|line| {
            line.split_once(". ")
                .is_some_and(|(position, _)| position.parse::<usize>().is_ok())
        })
        .collect()
}

async fn reply(registry: &CommandRegistry, name: &str, args: &str) -> String {
    match registry.dispatch(&CommandInvocation::new(name, args)).await {
        CommandOutcome::Reply(text) => text,
        other => panic!("/{name} {args} should reply: {other:?}"),
    }
}

async fn error(registry: &CommandRegistry, name: &str, args: &str) -> String {
    match registry.dispatch(&CommandInvocation::new(name, args)).await {
        CommandOutcome::Error(text) => text,
        other => panic!("/{name} {args} should fail: {other:?}"),
    }
}

async fn control(registry: &CommandRegistry, name: &str, args: &str) -> String {
    match registry.dispatch(&CommandInvocation::new(name, args)).await {
        CommandOutcome::Control(text) => text,
        other => panic!("/{name} {args} should schedule: {other:?}"),
    }
}

#[tokio::test]
async fn the_picker_shows_what_a_session_is_recognised_by() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(
        &store,
        "9f3c1a2b-6d40-4f19-9c11-1f2a3b4c5d6e",
        7,
        "2026-09-20T09:30:00Z",
        Some("fix the flaky retry test in the scheduler"),
    )
    .await;
    save(&store, "default", 2, "2026-09-18T14:05:00Z", Some("hello")).await;
    save(&store, "b21ec0de", 1, "2026-09-21T08:00:00Z", None).await;
    let (_navigation, registry) = navigate(&store, "default");

    let text = reply(&registry, "sessions", "").await;
    let rows = rows(&text);
    assert_eq!(rows.len(), 3, "one numbered row per saved session: {text}");
    assert!(rows[0].starts_with("1. `b21ec0de`"), "newest first: {text}");
    assert!(
        rows[2].starts_with("3. `default`") && rows[2].contains("(current)"),
        "the active session is marked where it sorts: {text}"
    );
    assert!(
        rows[1].contains("7 turns"),
        "a row says how much work is in the session: {text}"
    );
    assert!(
        rows[1].contains("2026-09-20 09:30 UTC"),
        "a row says when the session started: {text}"
    );
    assert!(
        rows[1].contains("fix the flaky retry test in the scheduler"),
        "a row says what the session was opened to do: {text}"
    );
    assert!(
        rows[0].contains("1 turn") && !rows[0].contains("1 turns"),
        "turn counts read naturally: {text}"
    );
    assert!(
        text.contains("/resume <number>"),
        "the list says how to choose from it: {text}"
    );
}

#[tokio::test]
async fn choosing_a_number_resumes_that_session() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "older", 3, "2026-09-19T10:00:00Z", None).await;
    save(&store, "newer", 1, "2026-09-21T10:00:00Z", None).await;
    let (navigation, registry) = navigate(&store, "default");

    let listed = reply(&registry, "sessions", "").await;
    assert!(listed.contains("2. `older`"), "{listed}");

    let scheduled = control(&registry, "resume", "2").await;
    assert!(
        scheduled.contains("`older`"),
        "the scheduled switch names the chosen session: {scheduled}"
    );
    assert_eq!(
        navigation.take_pending(),
        Some(SessionAction::Resume(SessionId::from_string("older"))),
        "the choice travels the existing scheduling path"
    );

    // What the composition root does at the boundary: the session that becomes
    // current is the one chosen, not the one that happened to sort first.
    navigation.set_current(SessionId::from_string("older"));
    assert_eq!(navigation.current().as_str(), "older");
}

#[tokio::test]
async fn bare_resume_opens_the_picker_instead_of_demanding_an_identifier() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "yesterday", 4, "2026-09-20T10:00:00Z", None).await;
    let (_navigation, registry) = navigate(&store, "default");

    let text = reply(&registry, "resume", "").await;
    assert!(text.contains("1. `yesterday`"), "{text}");
    assert!(text.contains("/resume <number>"), "{text}");
}

#[tokio::test]
async fn an_empty_repository_points_at_new_from_both_entry_points() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    let (_navigation, registry) = navigate(&store, "default");

    let listed = reply(&registry, "sessions", "").await;
    assert!(listed.contains("`/new`"), "{listed}");
    assert!(!listed.contains("1. "), "nothing to number: {listed}");

    let chosen = error(&registry, "resume", "1").await;
    assert!(chosen.contains("`/new`"), "{chosen}");
}

#[tokio::test]
async fn choosing_the_session_already_active_is_refused_before_it_is_scheduled() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "default", 2, "2026-09-21T10:00:00Z", None).await;
    save(&store, "other", 2, "2026-09-19T10:00:00Z", None).await;
    let (navigation, registry) = navigate(&store, "default");

    let refused = error(&registry, "resume", "1").await;
    assert!(
        refused.contains("already active"),
        "choosing the current row says so: {refused}"
    );
    assert_eq!(
        navigation.take_pending(),
        None,
        "a refused choice schedules nothing"
    );
}

#[tokio::test]
async fn a_number_off_the_end_of_the_list_says_what_the_range_is() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "only", 1, "2026-09-21T10:00:00Z", None).await;
    let (navigation, registry) = navigate(&store, "default");

    for typed in ["0", "2", "99"] {
        let refused = error(&registry, "resume", typed).await;
        assert!(
            refused.contains("1–1") && refused.contains("/sessions"),
            "`/resume {typed}` names the range and where to look: {refused}"
        );
    }
    assert_eq!(navigation.take_pending(), None);
}

#[tokio::test]
async fn a_session_whose_identifier_is_a_number_is_still_reachable_by_it() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "2", 1, "2026-09-19T10:00:00Z", None).await;
    save(&store, "newer", 1, "2026-09-21T10:00:00Z", None).await;
    let (navigation, registry) = navigate(&store, "default");

    // Row 2 is `2` itself here, but the identifier wins so a user who knows an
    // id never has to count rows to use it.
    save(&store, "middle", 1, "2026-09-20T10:00:00Z", None).await;
    let listed = reply(&registry, "sessions", "").await;
    assert!(listed.contains("2. `middle`"), "{listed}");

    control(&registry, "resume", "2").await;
    assert_eq!(
        navigation.take_pending(),
        Some(SessionAction::Resume(SessionId::from_string("2")))
    );
}

#[tokio::test]
async fn a_long_list_is_capped_and_says_how_many_it_left_out() {
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    for index in 0..25u32 {
        save(
            &store,
            &format!("session-{index:02}"),
            index,
            &format!("2026-09-{:02}T10:00:00Z", index + 1),
            None,
        )
        .await;
    }
    let (navigation, registry) = navigate(&store, "default");

    let text = reply(&registry, "sessions", "").await;
    assert_eq!(rows(&text).len(), 20, "the picker stays readable: {text}");
    assert!(
        text.contains("5 older sessions"),
        "it says what it left out: {text}"
    );
    assert!(
        text.contains("/resume <id>"),
        "and how to reach them anyway: {text}"
    );

    // Row 1 is the newest; row 20 is the oldest shown; row 21 is not addressable.
    control(&registry, "resume", "1").await;
    assert_eq!(
        navigation.take_pending(),
        Some(SessionAction::Resume(SessionId::from_string("session-24")))
    );
    let refused = error(&registry, "resume", "21").await;
    assert!(refused.contains("1–20"), "{refused}");
}

#[tokio::test]
async fn a_picker_without_a_store_reader_still_lists_and_still_chooses() {
    // `with_store` is what supplies the recognition facts. Without it the
    // command degrades to identifiers, and choosing by number must still work.
    let store: Arc<dyn SessionStore> = Arc::new(InMemoryStore::new());
    save(&store, "alpha", 1, "2026-09-21T10:00:00Z", None).await;
    save(&store, "beta", 1, "2026-09-20T10:00:00Z", None).await;
    let navigation = SessionNavigation::new(
        SessionsAPI::new(store.clone()),
        SessionId::from_string("default"),
    );
    let registry = CommandRegistry::with_builtins();
    navigation.register(&registry).unwrap();

    let text = reply(&registry, "sessions", "").await;
    assert!(
        text.contains("1. `alpha`") && text.contains("2. `beta`"),
        "{text}"
    );
    assert!(!text.contains("turn"), "no facts to show: {text}");

    control(&registry, "resume", "2").await;
    assert_eq!(
        navigation.take_pending(),
        Some(SessionAction::Resume(SessionId::from_string("beta")))
    );
}
