//! Terminal presentation proof for the daily coding workflow.
//!
//! The coding-host half lives in `polaris_code/tests/terminal_workflow.rs`.
//! These tests stay at the generic TUI boundary: real composer events, the
//! bounded IO channel, transcript rendering, resize reflow, Unicode cell
//! widths, and browse/search selection on a long transcript.

use polaris::plugins::{IOContent, IOMessage, IOProvider};
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;
use polaris_tui::ratatui::buffer::CellWidth as _;
use polaris_tui::{EventOutcome, TuiEvent, TuiPanel};

fn key(panel: &mut ConversationPanel, code: KeyCode, modifiers: KeyModifiers) -> EventOutcome {
    panel.handle_event(&TuiEvent::Key(KeyEvent::new(code, modifiers)))
}

async fn render(panel: &mut ConversationPanel, width: u16, height: u16) -> String {
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
        .map(|cells| {
            let mut row = String::new();
            let mut skip = 0u16;
            for cell in cells {
                if skip == 0 {
                    row.push_str(cell.symbol());
                }
                skip = skip.max(cell.cell_width()).saturating_sub(1);
            }
            row
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn multiline_unicode_composition_survives_narrow_resize_and_delivery() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
    let prompt = "first line\n你好 🌍 café\nthird line";
    assert_eq!(
        panel.handle_event(&TuiEvent::Paste(prompt.into())),
        EventOutcome::Consumed
    );

    let narrow = render(&mut panel, 24, 8).await;
    assert!(narrow.contains("first line"), "{narrow}");
    assert!(narrow.contains("你好"), "wide glyphs survive:\n{narrow}");
    assert!(narrow.contains("third line"), "{narrow}");

    panel.handle_event(&TuiEvent::Resize { cols: 80, rows: 20 });
    let wide = render(&mut panel, 80, 20).await;
    assert!(
        wide.contains("你好 🌍 café"),
        "resize reflows without loss:\n{wide}"
    );

    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    let delivered = provider.receive().await.unwrap();
    match delivered.content {
        IOContent::Text(text) => assert_eq!(text, prompt),
        other => panic!("expected text prompt, got {other:?}"),
    }
}

#[tokio::test]
async fn long_transcript_browse_search_selects_unicode_without_submitting() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(512);
    for index in 0..180 {
        let body = if index == 73 {
            "needle 世界\n```rust\nfn answer() -> u32 { 42 }\n```".to_string()
        } else {
            format!("transcript row {index:03}")
        };
        provider
            .send(IOMessage::from_agent("code", IOContent::Text(body)))
            .await
            .unwrap();
    }
    let tail = render(&mut panel, 52, 12).await;
    assert!(
        tail.contains("transcript row 179"),
        "tail remains visible:\n{tail}"
    );

    assert_eq!(
        key(&mut panel, KeyCode::Char('g'), KeyModifiers::CONTROL),
        EventOutcome::Consumed
    );
    assert_eq!(
        key(&mut panel, KeyCode::Char('/'), KeyModifiers::NONE),
        EventOutcome::Consumed
    );
    for ch in "needle".chars() {
        key(&mut panel, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    let found = render(&mut panel, 52, 12).await;
    assert!(
        found.contains("needle 世界"),
        "search reveals the match:\n{found}"
    );
    assert!(
        found.contains("Find: needle"),
        "browse mode remains explicit:\n{found}"
    );

    key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut panel, KeyCode::Up, KeyModifiers::NONE);
    let selected = render(&mut panel, 52, 12).await;
    assert!(
        selected.contains("Selected message 73"),
        "selection affordance remains visible:\n{selected}"
    );

    // Browse/search input is presentation-only and never enters UserIO.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), provider.receive())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn streamed_and_control_messages_remain_distinct_after_resize() {
    let (provider, mut panel) = TuiIOProvider::new_with_panel(16);
    provider
        .send(
            IOMessage::from_agent("code", IOContent::Text("partial ".into()))
                .with_metadata("io_type", "message")
                .with_metadata("id", "answer-1")
                .with_metadata("seq", "1")
                .with_metadata("final", "false"),
        )
        .await
        .unwrap();
    let partial = render(&mut panel, 30, 8).await;
    assert!(
        partial.contains("partial"),
        "a delta is visible immediately:\n{partial}"
    );

    provider
        .send(
            IOMessage::from_agent("code", IOContent::Text("partial complete".into()))
                .with_metadata("io_type", "message")
                .with_metadata("id", "answer-1")
                .with_metadata("seq", "2")
                .with_metadata("final", "true")
                .with_metadata("outcome", "complete"),
        )
        .await
        .unwrap();
    provider
        .send(
            IOMessage::from_agent("code", IOContent::Text("Preparing context…".into()))
                .with_metadata("io_type", "status"),
        )
        .await
        .unwrap();
    panel.handle_event(&TuiEvent::Resize { cols: 18, rows: 8 });
    let final_screen = render(&mut panel, 18, 8).await;
    assert_eq!(final_screen.matches("partial").count(), 1);
    assert_eq!(final_screen.matches("complete").count(), 1);
    assert!(
        final_screen.contains("Preparing"),
        "status remains separate:\n{final_screen}"
    );
}
