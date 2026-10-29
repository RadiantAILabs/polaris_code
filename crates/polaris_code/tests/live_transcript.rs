//! Proof that the agent's answer and its tool activity reach the terminal
//! *while they happen*, and that nothing the stream does can duplicate,
//! reorder or strand what the user sees.
//!
//! This drives the production emitters — [`forward`], [`LiveTranscript`] and
//! [`ToolActivity`] — over the real [`UserIO`] bridge into the real
//! [`ConversationPanel`], and asserts on the rows a terminal would actually
//! draw. There is no model and no network: provider events are scripted
//! [`StreamEvent`]s, so a failure means the forwarding contract broke, not
//! that a provider changed.
//!
//! What is deliberately *not* proven here: live subprocess stdout. A
//! `run_command` block goes `start` → `ok` with its captured result, because
//! process-level streaming is covered by `managed_shell_consumer.rs`.

use std::sync::Arc;

use polaris::models::llm::{ContentBlockDelta, StopReason, StreamEvent, Usage};
use polaris::plugins::UserIO;
use polaris::plugins::{IO_TYPE, IOContent, IOMessage};
use polaris_code::terminal::transcript::{
    FINAL_TRUE, IO_TYPE_MESSAGE, LiveTranscript, META_FINAL, META_ID, META_OUTCOME, META_SEQ,
    MessageHandle, OUTCOME_COMPLETE, OUTCOME_ERROR, PHASE_DENIED, PHASE_ERROR, PHASE_OK,
    PHASE_PENDING, PHASE_START, ToolActivity, forward, tool_detail,
};
use polaris_tui::TuiPanel;
use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
use polaris_tui::ratatui::Terminal;
use polaris_tui::ratatui::backend::TestBackend;

/// The agent's side of the bridge plus the panel the user is looking at.
fn bridge() -> (UserIO, ConversationPanel) {
    let (provider, panel) = TuiIOProvider::new_with_panel(256);
    (UserIO::new(Arc::new(provider)), panel)
}

/// A scripted text delta, exactly as a provider emits one.
fn text_delta(text: &str) -> Result<StreamEvent, std::io::Error> {
    Ok(StreamEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Text(text.to_string()),
    })
}

/// A scripted reasoning delta — visible in the deep view, never the transcript.
fn reasoning_delta(text: &str) -> Result<StreamEvent, std::io::Error> {
    Ok(StreamEvent::ContentBlockDelta {
        index: 0,
        delta: ContentBlockDelta::Reasoning(text.to_string()),
    })
}

/// A scripted tool-argument fragment: partial JSON, meaningless until it stops.
fn tool_arg_delta(fragment: &str) -> Result<StreamEvent, std::io::Error> {
    Ok(StreamEvent::ContentBlockDelta {
        index: 1,
        delta: ContentBlockDelta::ToolCall {
            arguments: fragment.to_string(),
        },
    })
}

fn stop() -> Result<StreamEvent, std::io::Error> {
    Ok(StreamEvent::MessageStop {
        stop_reason: StopReason::EndTurn,
        usage: Usage::default(),
        id: Some("provider-assigned-id".into()),
        model: None,
    })
}

fn stream_error() -> Result<StreamEvent, std::io::Error> {
    Err(std::io::Error::other("connection reset"))
}

/// Draw the panel and return the rows a terminal would show.
///
/// The provider hands agent→user messages to the panel across a spawned pump
/// task, so the test yields until that task has drained everything already
/// sent before ticking. Yielding (rather than sleeping) keeps the test
/// deterministic: on a current-thread runtime each yield is one step of the
/// pump, not a race against a clock.
async fn screen(panel: &mut ConversationPanel) -> String {
    for _ in 0..256 {
        tokio::task::yield_now().await;
    }
    panel.tick();
    let (width, height) = (80u16, 40u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            panel.render(frame, area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Stream `chunks` through the production forwarder, reporting whether the
/// attempt became committed.
async fn stream(block: &MessageHandle, io: &UserIO, chunks: &[&str]) -> bool {
    let mut committed = false;
    for chunk in chunks {
        committed |= forward(block, io, &text_delta(chunk)).await;
    }
    committed
}

#[tokio::test]
async fn text_appears_incrementally_rather_than_all_at_the_end() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();

    forward(&block, &io, &text_delta("Looking ")).await;
    // Mid-stream, before anything has finished: the user can already read it.
    assert!(
        screen(&mut panel).await.contains("Looking"),
        "the first delta must be on screen before the message ends"
    );

    forward(&block, &io, &text_delta("at that now.")).await;
    live.close(&io, "Looking at that now.", OUTCOME_COMPLETE)
        .await;

    let screen = screen(&mut panel).await;
    assert!(screen.contains("Looking at that now."), "{screen}");
}

#[tokio::test]
async fn only_text_is_forwarded_to_the_transcript() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();

    assert!(!forward(&block, &io, &reasoning_delta("I should check x")).await);
    assert!(!forward(&block, &io, &tool_arg_delta("{\"path\":\"a")).await);
    assert!(!forward(&block, &io, &stop()).await);
    assert!(
        forward(&block, &io, &text_delta("Here is the answer.")).await,
        "text is what commits an attempt"
    );

    let screen = screen(&mut panel).await;
    assert!(screen.contains("Here is the answer."), "{screen}");
    assert!(
        !screen.contains("I should check x"),
        "reasoning belongs to the Ctrl-O deep view, not the transcript: {screen}"
    );
    assert!(
        !screen.contains("path"),
        "a partial tool-argument fragment must never render: {screen}"
    );
}

#[tokio::test]
async fn a_duplicate_final_cannot_print_the_answer_twice() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();
    let id = block.id().to_string();
    stream(&block, &io, &["The ", "answer."]).await;
    live.close(&io, "The answer.", OUTCOME_COMPLETE).await;

    // A resend of the closing message, byte for byte — the shape a retried
    // send takes on the wire. Its `seq` does not advance the block, so the
    // panel drops it.
    io.send(
        IOMessage::from_agent("code", IOContent::Text("The answer.".into()))
            .with_metadata(IO_TYPE, IO_TYPE_MESSAGE)
            .with_metadata(META_ID, id)
            .with_metadata(META_SEQ, "2")
            .with_metadata(META_FINAL, FINAL_TRUE)
            .with_metadata(META_OUTCOME, OUTCOME_COMPLETE),
    )
    .await
    .unwrap();

    let screen = screen(&mut panel).await;
    assert_eq!(
        screen.matches("The answer.").count(),
        1,
        "the answer must appear once: {screen}"
    );
}

#[tokio::test]
async fn a_failed_stream_keeps_its_partial_text_and_marks_the_outcome() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();

    stream(&block, &io, &["I was part way through"]).await;
    // The provider dies mid-message.
    assert!(!forward(&block, &io, &stream_error()).await);
    live.close(&io, "", OUTCOME_ERROR).await;

    let screen = screen(&mut panel).await;
    assert!(
        screen.contains("I was part way through"),
        "partial text must survive the failure: {screen}"
    );
    assert!(
        screen.contains("response ended early"),
        "and the block must say it stopped: {screen}"
    );
}

#[tokio::test]
async fn a_retry_after_visible_text_is_refused_and_a_silent_one_is_clean() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();

    // Attempt 1 fails before emitting anything: nothing is committed, so the
    // retry is safe and leaves no empty block behind.
    let first = live.open();
    assert!(!forward(&first, &io, &stream_error()).await);
    assert!(!first.opened(), "a silent attempt shows nothing");
    live.close(&io, "", OUTCOME_ERROR).await;

    // Attempt 2 streams, so it is committed and owns its own block.
    let second = live.open();
    assert!(stream(&second, &io, &["Second attempt speaking."]).await);
    live.close(&io, "Second attempt speaking.", OUTCOME_COMPLETE)
        .await;

    let screen = screen(&mut panel).await;
    assert_eq!(
        screen.matches("Second attempt speaking.").count(),
        1,
        "{screen}"
    );
    assert!(
        !screen.contains("response ended early"),
        "the silent failed attempt must not leave a marked empty block: {screen}"
    );
}

#[tokio::test]
async fn tool_activity_interleaves_under_the_message_that_asked_for_it() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();
    stream(&block, &io, &["Reading both files."]).await;

    let args = serde_json::json!({ "path": "src/lib.rs" });
    let first = ToolActivity::start(
        &io,
        "call_1",
        live.current_id(),
        "read_file",
        &tool_detail("read_file", &args),
    )
    .await;
    let second =
        ToolActivity::start(&io, "call_2", live.current_id(), "read_file", "src/main.rs").await;
    // They finish out of order; each must land on its own block.
    second.finish(&io, PHASE_OK, "fn main() {}").await;
    first.finish(&io, PHASE_OK, "pub mod terminal;").await;
    live.close(&io, "Reading both files.", OUTCOME_COMPLETE)
        .await;

    let screen = screen(&mut panel).await;
    assert!(screen.contains("Reading both files."), "{screen}");
    assert!(
        screen.contains("src/lib.rs") || screen.contains("pub mod terminal;"),
        "{screen}"
    );
    assert!(screen.contains("fn main() {}"), "{screen}");
    assert_eq!(
        screen.matches("read_file").count(),
        2,
        "two invocations, two rows: {screen}"
    );
}

#[tokio::test]
async fn an_approval_denial_ends_the_tool_block_as_denied() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    let block = live.open();
    stream(&block, &io, &["I'll remove that."]).await;

    let activity =
        ToolActivity::start(&io, "call_1", live.current_id(), "run_command", "rm -rf /").await;
    activity.phase(&io, PHASE_PENDING, "rm -rf /").await;
    activity
        .finish(
            &io,
            PHASE_DENIED,
            "User denied execution of tool 'run_command'",
        )
        .await;

    let screen = screen(&mut panel).await;
    assert!(screen.contains("User denied execution"), "{screen}");
    // One block, carrying only its terminal phase: the `start` and `pending`
    // updates replaced it in place rather than leaving rows behind.
    assert_eq!(
        screen.matches("⊘ run_command").count(),
        1,
        "start, pending and denied are one block, not three: {screen}"
    );
    assert!(!screen.contains("⋯ run_command"), "{screen}");
    assert!(!screen.contains("… run_command"), "{screen}");
}

#[tokio::test]
async fn a_failing_tool_ends_its_own_block_without_a_separate_error_line() {
    let (io, mut panel) = bridge();
    let activity = ToolActivity::start(&io, "call_1", None, "edit_file", "src/lib.rs").await;
    activity.phase(&io, PHASE_START, "src/lib.rs").await;
    activity
        .finish(&io, PHASE_ERROR, "old_string not found in src/lib.rs")
        .await;

    let screen = screen(&mut panel).await;
    assert!(screen.contains("old_string not found"), "{screen}");
    assert_eq!(
        screen.matches("✗ edit_file").count(),
        1,
        "a failed tool reports in its own block: {screen}"
    );
}

#[tokio::test]
async fn a_large_tool_result_is_bounded_on_screen() {
    let (io, mut panel) = bridge();
    let activity = ToolActivity::start(&io, "call_1", None, "run_command", "cargo test").await;
    let huge = "a line of captured output\n".repeat(5_000);
    activity.finish(&io, PHASE_OK, &huge).await;

    let screen = screen(&mut panel).await;
    assert!(
        screen.contains("Ctrl-O to expand"),
        "a large result must collapse with a way to see the rest: {screen}"
    );
    assert!(
        screen.lines().count() <= 40,
        "the panel must not try to draw it all"
    );
}

#[tokio::test]
async fn a_pure_tool_turn_leaves_no_empty_message_block() {
    let (io, mut panel) = bridge();
    let mut live = LiveTranscript::default();
    live.open();
    // The model went straight to a tool with no preamble.
    let activity = ToolActivity::start(&io, "call_1", live.current_id(), "search", "needle").await;
    activity.finish(&io, PHASE_OK, "3 matches").await;
    live.close(&io, "", OUTCOME_COMPLETE).await;

    let screen = screen(&mut panel).await;
    assert!(screen.contains("search"), "{screen}");
    assert!(
        !screen.contains("code ❯"),
        "no empty assistant block should be drawn: {screen}"
    );
}

#[tokio::test]
async fn a_plain_consumer_is_unaffected() {
    // The compatibility invariant, end to end: a producer that stamps none of
    // this vocabulary renders exactly as it always has.
    let (provider, mut panel) = TuiIOProvider::new_with_panel(8);
    let io = UserIO::new(Arc::new(provider));
    io.send(IOMessage::from_agent(
        "code",
        IOContent::Text("just a message".into()),
    ))
    .await
    .unwrap();

    let screen = screen(&mut panel).await;
    assert!(screen.contains("just a message"), "{screen}");
}
