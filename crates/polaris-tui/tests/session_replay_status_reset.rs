//! Session-boundary proof: a replay must not leave the previous session's
//! status facts, error banner, or unavailability on screen.
//!
//! These drive the real panel through the public host surface — a
//! [`TuiIOProvider`] channel, `io_type` messages built by the public
//! `commands` helpers, real key events, and a `TestBackend` render — and
//! assert on the **rendered rows**, because the defect this guards is about
//! what the user is shown, not about which field happens to hold it.
//!
//! The panel's own unit tests cover the field-level reset; this file is the
//! user-visible half, and it is the half that fails when a future change
//! stops clearing session-scoped state on replay.

use polaris::plugins::{IOMessage, IOProvider};
use polaris_tui::TuiPanel;
use polaris_tui::commands::{
    Availability, ContextWindow, StatusReport, report_status, report_unavailable,
    session_replay_end, session_replay_start,
};
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{EventOutcome, TuiEvent};

/// Wide enough that the meta row draws every fact it has *and* has room to
/// print them: the model, context and cost facts need 40 columns and the
/// workspace, session and permission labels need 70, but the row also carries
/// the activity word, the token tally and a hidden-row count, and anything
/// past the last column is simply not on screen.
const WIDE: u16 = 180;

/// Everything the first session reported. Each value is deliberately
/// unmistakable, so a row containing it can only have come from here.
fn first_session_report() -> StatusReport {
    StatusReport::default()
        .with_model("first-model-zzz")
        .with_workspace("first-workspace-zzz")
        .with_session("first-session-zzz")
        .with_permissions("first-permissions-zzz")
        .with_context(ContextWindow::new(90_000).with_limit(100_000))
        .with_cost_usd(12.3456)
}

/// The labels `first_session_report` asserts on, plus the `90%` ratio the
/// context meter derives from it.
const FIRST_SESSION_FACTS: [&str; 6] = [
    "first-model-zzz",
    "first-workspace-zzz",
    "first-session-zzz",
    "first-permissions-zzz",
    "12.3456",
    "90%",
];

async fn render(panel: &mut ConversationPanel, width: u16, height: u16) -> String {
    // The provider hands outbound messages to the panel through a spawned
    // drain task; yielding lets it run before the panel reads its inbox.
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
    panel.tick();
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(width as usize)
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Replace the visible session, exactly as a host does: the start event, the
/// canonical history, then the end event.
async fn replay_into(provider: &TuiIOProvider, id: &str, history: &[&str]) {
    provider.send(session_replay_start(id)).await.unwrap();
    for text in history {
        provider
            .send(IOMessage::from_agent(
                "agent",
                polaris::plugins::IOContent::Text((*text).to_string()),
            ))
            .await
            .unwrap();
    }
    provider.send(session_replay_end(id)).await.unwrap();
}

#[tokio::test]
async fn replay_does_not_carry_the_previous_sessions_status_facts() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(32);

    provider
        .send(report_status(&first_session_report()))
        .await
        .unwrap();
    let before = render(&mut panel, WIDE, 16).await;
    for fact in FIRST_SESSION_FACTS {
        assert!(
            before.contains(fact),
            "the first session's own report must render, missing {fact}:\n{before}"
        );
    }

    replay_into(&provider, "second", &["restored turn"]).await;
    let after = render(&mut panel, WIDE, 16).await;

    assert!(
        after.contains("restored turn"),
        "the replayed transcript must be on screen:\n{after}"
    );
    for fact in FIRST_SESSION_FACTS {
        assert!(
            !after.contains(fact),
            "the replayed session still shows the previous session's {fact}:\n{after}"
        );
    }
    assert!(
        after.contains("ready"),
        "with nothing reported yet the meta row says ready:\n{after}"
    );
}

#[tokio::test]
async fn replay_does_not_carry_the_previous_sessions_error_banner() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(32);

    provider
        .send(IOMessage::system_text("first-session-blew-up-zzz").with_metadata("io_type", "error"))
        .await
        .unwrap();
    let before = render(&mut panel, WIDE, 16).await;
    assert!(
        before.contains("error · first-session-blew-up-zzz"),
        "the first session's failure must reach the meta row:\n{before}"
    );

    replay_into(&provider, "second", &["restored turn"]).await;
    let after = render(&mut panel, WIDE, 16).await;

    assert!(
        !after.contains("error · first-session-blew-up-zzz"),
        "the replayed session still shows the previous session's failure:\n{after}"
    );
    assert!(
        after.contains("ready"),
        "a session with no reported failure reads as ready:\n{after}"
    );
}

#[tokio::test]
async fn replay_does_not_carry_the_previous_sessions_unavailability() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(32);

    provider
        .send(report_unavailable("first-session-is-down-zzz"))
        .await
        .unwrap();
    let blocked = render(&mut panel, WIDE, 16).await;
    assert!(
        blocked.contains("first-session-is-down-zzz"),
        "the host's own unavailability must show:\n{blocked}"
    );

    // Unavailability is not cosmetic: it refuses submissions outright.
    assert_eq!(
        panel.handle_event(&TuiEvent::Paste("held back".into())),
        EventOutcome::Consumed
    );
    panel.handle_event(&TuiEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let refused = render(&mut panel, WIDE, 16).await;
    assert!(
        refused.contains("the host cannot process work"),
        "a submission against an unavailable host is refused:\n{refused}"
    );

    replay_into(&provider, "second", &["restored turn"]).await;
    let after = render(&mut panel, WIDE, 16).await;

    assert!(
        !after.contains("first-session-is-down-zzz"),
        "the replayed session still claims the previous host was down:\n{after}"
    );
    assert!(
        !after.contains("the host cannot process work"),
        "the refusal banner outlived the session it described:\n{after}"
    );

    // And the refusal is genuinely lifted, not merely hidden: the same draft
    // now reaches the agent.
    panel.handle_event(&TuiEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
    let delivered = provider.receive().await.unwrap();
    match delivered.content {
        polaris::plugins::IOContent::Text(text) => assert_eq!(
            text, "held back",
            "the draft held back by the previous session's posture is deliverable"
        ),
        other => panic!("expected the held-back draft as text, got {other:?}"),
    }
}

#[tokio::test]
async fn a_replayed_session_reports_its_own_status_from_scratch() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(32);

    provider
        .send(report_status(&first_session_report()))
        .await
        .unwrap();
    replay_into(&provider, "second", &["restored turn"]).await;

    // A partial report is the normal case: a host re-sends what changed. The
    // fields it omits must read as unknown, not as the previous session's.
    provider
        .send(report_status(
            &StatusReport::default()
                .with_model("second-model-zzz")
                .with_availability(Availability::Available),
        ))
        .await
        .unwrap();
    let after = render(&mut panel, WIDE, 16).await;

    assert!(
        after.contains("second-model-zzz"),
        "the new session's own model must show:\n{after}"
    );
    for fact in FIRST_SESSION_FACTS {
        assert!(
            !after.contains(fact),
            "a partial report re-merged {fact} from the previous session:\n{after}"
        );
    }
}
