//! The operator's permission ceiling, from inside a session.
//!
//! The ceiling is an invariant of [`SessionSettings`], the one mutable holder
//! of a session's permission mode, so these tests drive the real in-session
//! routes to a posture — the `/permission` command as the TUI dispatches it,
//! and the `/new`/`/resume` reset — rather than asserting on the comparison
//! helper the unit tests in `agents::policy` already cover.

use polaris_code::agents::{PermissionCeiling, PermissionMode};
use polaris_code::config::ModelCatalog;
use polaris_code::terminal::{CommandOutcome, CommandRegistry, SessionSettings};
use polaris_tui::commands::CommandInvocation;

/// The model set is irrelevant to a permission ceiling; one entry keeps
/// `/model` registrable without saying anything about the posture.
fn catalog() -> ModelCatalog {
    ModelCatalog::new("fake/one", ["fake/one"])
}

fn ceiling(mode: PermissionMode) -> PermissionCeiling {
    PermissionCeiling::new(mode).expect("an attended posture is a ceiling")
}

/// A session started in `start`, held under `limit`, with its commands
/// registered exactly as the host registers them.
fn session(
    start: PermissionMode,
    limit: Option<PermissionCeiling>,
) -> (CommandRegistry, SessionSettings) {
    let settings = SessionSettings::new("fake/one", start, &catalog());
    let settings = match limit {
        Some(limit) => settings.with_permission_ceiling(limit),
        None => settings,
    };
    let commands = CommandRegistry::with_builtins();
    settings.register(&commands).expect("commands register");
    (commands, settings)
}

fn permission_spec_choices(commands: &CommandRegistry) -> Vec<String> {
    commands
        .specs()
        .into_iter()
        .find(|spec| spec.name == "permission")
        .expect("`/permission` is registered")
        .choices
}

fn permission_spec_description(commands: &CommandRegistry) -> String {
    commands
        .specs()
        .into_iter()
        .find(|spec| spec.name == "permission")
        .expect("`/permission` is registered")
        .description
}

/// The requirement: under a ceiling, every in-session route to a more
/// permissive posture is refused, and the refusal says a ceiling is in force
/// and what it is.
#[tokio::test]
async fn every_in_session_route_above_the_ceiling_is_refused_by_name() {
    let (commands, settings) =
        session(PermissionMode::Confirm, Some(ceiling(PermissionMode::Plan)));

    // Installing the ceiling lowers the posture the host asked for, so the
    // session is never even observed above it.
    assert_eq!(settings.snapshot().permission_mode, PermissionMode::Plan);

    for requested in ["confirm", "auto-edit"] {
        let outcome = commands
            .dispatch(&CommandInvocation::new("permission", requested))
            .await;
        let CommandOutcome::Error(refusal) = outcome else {
            panic!("`/permission {requested}` was not refused under a `plan` ceiling: {outcome:?}");
        };
        assert!(
            refusal.contains("ceiling"),
            "the refusal must say a ceiling is in force: {refusal}"
        );
        assert!(
            refusal.contains("plan"),
            "the refusal must name the ceiling: {refusal}"
        );
        assert!(
            refusal.contains(requested),
            "the refusal must name the posture it refused: {refusal}"
        );
        assert_eq!(
            settings.snapshot().permission_mode,
            PermissionMode::Plan,
            "a refused escalation must leave the posture it found"
        );
    }

    // Entering another session resets the defaults; it must not be a way out
    // one rung higher than the operator allowed.
    settings.reset("fake/one");
    assert_eq!(settings.snapshot().permission_mode, PermissionMode::Plan);
}

/// A clone of the settings taken before the ceiling was installed shares the
/// same gate: the ceiling is a property of the session, not of one handle to
/// it. Without this the host could hand a pre-ceiling copy to a command
/// registry and the invariant would be one clone wide.
#[tokio::test]
async fn a_handle_taken_before_the_ceiling_is_still_held_by_it() {
    let settings = SessionSettings::new("fake/one", PermissionMode::Confirm, &catalog());
    let earlier = settings.clone();
    let _held = settings.with_permission_ceiling(ceiling(PermissionMode::Confirm));

    let commands = CommandRegistry::with_builtins();
    earlier.register(&commands).expect("commands register");

    assert!(
        matches!(
            commands
                .dispatch(&CommandInvocation::new("permission", "auto-edit"))
                .await,
            CommandOutcome::Error(_)
        ),
        "a pre-ceiling handle escaped the gate"
    );
    assert_eq!(earlier.snapshot().permission_mode, PermissionMode::Confirm);
}

/// The ceiling constrains only what is above it. Everything at or below stays
/// as freely selectable as it was without one.
#[tokio::test]
async fn postures_at_or_below_the_ceiling_stay_freely_selectable() {
    let (commands, settings) = session(
        PermissionMode::Confirm,
        Some(ceiling(PermissionMode::AutoEdit)),
    );

    for (requested, expected) in [
        ("plan", PermissionMode::Plan),
        ("confirm", PermissionMode::Confirm),
        ("auto-edit", PermissionMode::AutoEdit),
    ] {
        let outcome = commands
            .dispatch(&CommandInvocation::new("permission", requested))
            .await;
        assert!(
            matches!(outcome, CommandOutcome::Reply(_)),
            "`/permission {requested}` is at or below an `auto-edit` ceiling: {outcome:?}"
        );
        assert_eq!(settings.snapshot().permission_mode, expected);
    }
}

/// Visibility: a user who cannot raise their permissions reads the ceiling off
/// the ordinary `/permission` surface — the popup's choices and description,
/// and the no-argument reply — rather than discovering it by being refused.
#[tokio::test]
async fn the_session_shows_the_ceiling_without_being_asked() {
    let (commands, _settings) = session(
        PermissionMode::Confirm,
        Some(ceiling(PermissionMode::Confirm)),
    );

    assert_eq!(
        permission_spec_choices(&commands),
        ["confirm", "plan"],
        "the picker must offer only postures that can actually be entered"
    );
    let description = permission_spec_description(&commands);
    assert!(
        description.contains("ceiling") && description.contains("confirm"),
        "the command popup must say which ceiling narrowed it: {description}"
    );

    let outcome = commands
        .dispatch(&CommandInvocation::new("permission", ""))
        .await;
    let CommandOutcome::Reply(reply) = outcome else {
        panic!("`/permission` must report the active posture: {outcome:?}");
    };
    assert!(
        reply.contains("ceiling") && reply.contains("`confirm`"),
        "the status reply must name the ceiling in force: {reply}"
    );
}

/// Without a ceiling nothing narrows: the unconstrained session keeps every
/// posture and says nothing about a ceiling it does not have.
#[tokio::test]
async fn an_unconstrained_session_is_unchanged() {
    let (commands, settings) = session(PermissionMode::Confirm, None);

    assert_eq!(
        permission_spec_choices(&commands),
        ["confirm", "auto-edit", "plan"]
    );
    assert!(!permission_spec_description(&commands).contains("ceiling"));
    assert!(
        matches!(
            commands
                .dispatch(&CommandInvocation::new("permission", "auto-edit"))
                .await,
            CommandOutcome::Reply(_)
        ),
        "an unconstrained session must still reach `auto-edit`"
    );
    assert_eq!(
        settings.snapshot().permission_mode,
        PermissionMode::AutoEdit
    );
}
