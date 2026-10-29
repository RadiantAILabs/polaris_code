//! Public host contract for the background-task surface.
//!
//! Two optional items, checked at the boundary a consumer actually uses: the
//! `ConversationPanelPlugin` builder, and the `StatusReport` wire format the
//! panel renders from. The chord's key handling is unit-tested inside the
//! panel, which is where its private state lives; what these tests pin is the
//! part a host depends on — that installing either item is optional, that a
//! consumer which installs neither renders no task facts, and that the
//! count survives the round trip a real host's message makes.

use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris_tui::commands::{
    Availability, ContextWindow, StatusReport, parse_status, report_status,
};
use polaris_tui::panels::{ConversationPanel, ConversationPanelPlugin, TuiIOProvider};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::{TuiEvent, TuiPanel};

/// Render the panel and return its rows, after letting queued IO land.
async fn render(panel: &mut ConversationPanel, width: u16, height: u16) -> Vec<String> {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    panel.handle_event(&TuiEvent::Tick);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect()
}

/// A `tool` block update, as a coding host sends one.
fn tool(id: &str, label: &str, phase: &str, body: &str) -> IOMessage {
    IOMessage::from_agent("code", IOContent::Text(body.to_string()))
        .with_metadata("io_type", "tool")
        .with_metadata("id", id)
        .with_metadata("phase", phase)
        .with_metadata("label", label)
}

#[test]
fn a_host_may_install_either_item_both_or_neither() {
    // Each is independently optional: neither builder method requires the
    // other, and neither requires any of the handles already on the plugin.
    let _neither = ConversationPanelPlugin::default();
    let _background = ConversationPanelPlugin::default().with_background_handle(|| {});
    let _both = ConversationPanelPlugin::default()
        .with_background_handle(|| {})
        .with_interrupt_handle(|| {});
}

#[test]
fn the_running_count_round_trips_and_merges_without_touching_anything_else() {
    let full = StatusReport::default()
        .with_model("anthropic/claude-opus-5")
        .with_context(ContextWindow::new(18_000).with_limit(200_000))
        .with_cost_usd(0.0412)
        .with_availability(Availability::Available)
        .with_tasks_running(2);
    assert_eq!(parse_status(&report_status(&full)), Some(full.clone()));

    // A host that never sets it asserts nothing, and merging that report over
    // a live count must not silently drop the count.
    let quiet = StatusReport::default().with_model("other-model");
    assert_eq!(quiet.tasks_running, None);
    let mut cached = full.clone();
    cached.merge(quiet);
    assert_eq!(cached.tasks_running, Some(2));
    assert_eq!(cached.model.as_deref(), Some("other-model"));

    // Zero is an assertion, not silence: it retracts the count.
    cached.merge(StatusReport::default().with_tasks_running(0));
    assert_eq!(cached.tasks_running, Some(0));
}

#[tokio::test]
async fn a_consumer_using_neither_item_renders_without_task_facts() {
    // The plain-consumer check. Two panels are fed the same transcript and
    // the same status report; the only difference is that the second report
    // sets `tasks_running`. Every row other than the meta line must match,
    // and the first panel's meta line must not mention the new fact at all.
    let (plain_provider, mut plain) = TuiIOProvider::new_with_panel(8);
    let (extended_provider, mut extended) = TuiIOProvider::new_with_panel(8);

    let base = StatusReport::default()
        .with_model("anthropic/claude-opus-5")
        .with_cost_usd(0.0412)
        .with_availability(Availability::Available);
    for (provider, report) in [
        (&plain_provider, base.clone()),
        (&extended_provider, base.clone().with_tasks_running(2)),
    ] {
        provider
            .send(IOMessage::from_agent(
                "code",
                IOContent::Text("running the suite".into()),
            ))
            .await
            .unwrap();
        provider
            .send(tool("t0", "run_command", "start", "cargo test"))
            .await
            .unwrap();
        provider.send(report_status(&report)).await.unwrap();
    }

    let plain_rows = render(&mut plain, 80, 12).await;
    let extended_rows = render(&mut extended, 80, 12).await;

    let plain_meta = plain_rows.iter().find(|row| row.contains("ready")).cloned();
    let extended_meta = extended_rows
        .iter()
        .find(|row| row.contains("ready"))
        .cloned();
    assert!(plain_meta.is_some(), "{plain_rows:#?}");
    assert!(
        !plain_meta.clone().unwrap().contains("running"),
        "a host that never set the field says nothing about tasks: {plain_meta:?}"
    );
    assert!(
        extended_meta.clone().unwrap().contains("2 running"),
        "a host that set it gets one more dim fact: {extended_meta:?}"
    );

    let differing: Vec<(usize, &String, &String)> = plain_rows
        .iter()
        .zip(extended_rows.iter())
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(index, (a, b))| (index, a, b))
        .collect();
    assert_eq!(
        differing.len(),
        1,
        "only the meta row may differ, got {differing:#?}"
    );
    assert!(
        differing[0].1.contains("ready"),
        "the one differing row is the meta line: {differing:#?}"
    );
}

#[tokio::test]
async fn a_backgrounded_phase_needs_no_new_panel_vocabulary() {
    // `phase = "background"` is a host word. The panel has no arm for it and
    // needs none: an unrecognised phase renders with the fallthrough glyph,
    // in the same block, so a host can extend the lifecycle without a
    // `polaris-tui` release.
    let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
    provider
        .send(tool("t0", "run_command", "start", "cargo test --workspace"))
        .await
        .unwrap();
    provider
        .send(tool(
            "t0",
            "run_command",
            "background",
            "backgrounded as task 3 — /tasks lists it, /stop 3 ends it",
        ))
        .await
        .unwrap();

    let rows = render(&mut panel, 80, 12).await;
    let joined = rows.join("\n");
    assert!(
        joined.contains("backgrounded as task 3"),
        "the host's own words render verbatim:\n{joined}"
    );
    assert_eq!(
        joined.matches("run_command").count(),
        1,
        "one invocation stays one row:\n{joined}"
    );
}
