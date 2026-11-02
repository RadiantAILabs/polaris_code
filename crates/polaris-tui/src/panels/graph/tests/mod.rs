use super::*;
use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseEvent};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

/// A graph shaped like the agent's loop: a lead-in, a loop whose body holds a
/// two-way choice with a one-step branch and a many-step branch, a nested
/// choice, and an error handler beside the chain.
fn agent_like() -> GraphModel {
    let tools = GraphNode::new("has_tool_calls", "has_tool_calls", GraphNodeKind::Decision)
        .with_branch(GraphBranch::new(
            "yes",
            vec![
                GraphNode::system("review_tools", "review_tools"),
                GraphNode::system("execute_tools", "execute_tools"),
            ],
        ))
        .with_branch(GraphBranch::new(
            "no",
            vec![GraphNode::system("finalize", "finalize")],
        ));
    let compaction = GraphNode::new(
        "manual_compaction",
        "manual_compaction",
        GraphNodeKind::Decision,
    )
    .with_branch(GraphBranch::new(
        "yes",
        vec![GraphNode::system("finish_compaction", "finish_compaction")],
    ))
    .with_branch(GraphBranch::new(
        "no",
        vec![
            GraphNode::system("prepare_context", "prepare_context"),
            GraphNode::system("act", "act"),
            tools,
        ],
    ));
    let body = GraphNode::new("agent_loop", "agent_loop", GraphNodeKind::Loop)
        .with_note("repeats")
        .with_branch(GraphBranch::new(
            "",
            vec![
                GraphNode::system("apply_steering", "apply_steering"),
                compaction,
            ],
        ));
    GraphModel::new(
        "CodeAgent",
        vec![
            GraphNode::system("receive_user_input", "receive_user_input"),
            GraphNode::system("init_loop", "init_loop"),
            body,
        ],
    )
    .with_side_branch(GraphBranch::new(
        "on error",
        vec![GraphNode::system("recover", "recover")],
    ))
}

/// `count` systems in a row, `s00` … — enough to need scrolling.
fn long_chain(count: usize) -> GraphModel {
    GraphModel::new(
        "long",
        (0..count)
            .map(|i| GraphNode::system(format!("s{i:02}"), format!("s{i:02}")))
            .collect(),
    )
}

fn panel_over(model: GraphModel) -> (GraphPanel, GraphFeed) {
    let feed = GraphFeed::new();
    feed.publish(model);
    (GraphPanel::new(feed.clone()), feed)
}

fn draw(panel: &mut GraphPanel, width: u16, height: u16) -> Terminal<TestBackend> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("test terminal");
    terminal
        .draw(|frame| panel.render(frame, frame.area()))
        .expect("draw");
    terminal
}

fn text_of(buffer: &Buffer) -> Vec<String> {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn screen(panel: &mut GraphPanel, width: u16, height: u16) -> Vec<String> {
    let terminal = draw(panel, width, height);
    text_of(terminal.backend().buffer())
}

fn press(code: KeyCode) -> TuiEvent {
    TuiEvent::Key(KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn wheel(kind: MouseEventKind) -> TuiEvent {
    TuiEvent::Mouse(MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    })
}

/// The row of `screen` that mentions `needle`.
fn row_with<'a>(screen: &'a [String], needle: &str) -> &'a str {
    screen
        .iter()
        .find(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("no row mentions {needle:?} in:\n{}", screen.join("\n")))
}

/// The style of the label of the plain node `name`: the cell after its `• `
/// glyph on the row that draws it.
fn label_style(terminal: &Terminal<TestBackend>, name: &str) -> Style {
    let needle = format!("• {name}");
    let buffer = terminal.backend().buffer();
    for (y, line) in text_of(buffer).iter().enumerate() {
        if let Some(byte) = line.find(&needle) {
            let column = line[..byte].chars().count() + 2;
            return buffer[(column as u16, y as u16)].style();
        }
    }
    panic!("no row draws {needle:?}");
}

#[test]
fn a_representative_graph_is_drawn_as_an_outline_that_fits_80_by_24() {
    let (mut panel, _feed) = panel_over(agent_like());
    let screen = screen(&mut panel, 80, 24);

    assert!(
        screen[0].starts_with(" CodeAgent · 13 nodes"),
        "header names the graph and counts its nodes: {:?}",
        screen[0]
    );
    let body: Vec<&str> = screen[1..].iter().map(String::as_str).collect();
    assert_eq!(
        body[..16],
        [
            "  • receive_user_input",
            "  • init_loop",
            "  ↻ agent_loop  repeats",
            "    ├─ • apply_steering",
            "    └─ ? manual_compaction",
            "       ├─ yes → • finish_compaction",
            "       └─ no",
            "          ├─ • prepare_context",
            "          ├─ • act",
            "          └─ ? has_tool_calls",
            "             ├─ yes",
            "             │  ├─ • review_tools",
            "             │  └─ • execute_tools",
            "             └─ no → • finalize",
            "  on error",
            "    └─ • recover",
        ],
        "branches nest under their node, a one-step branch shares its label's row, \
         and a handler sits after the chain"
    );
    assert!(
        body[16..].iter().all(|row| row.is_empty()),
        "the whole graph fits, so nothing is cut or scrolled"
    );
}

#[test]
fn the_highlight_moves_to_the_node_that_starts() {
    let (mut panel, feed) = panel_over(agent_like());
    let _run = feed.begin_run();

    let first = feed.enter("act", "act");
    let terminal = draw(&mut panel, 80, 24);
    let lines = text_of(terminal.backend().buffer());
    assert!(
        row_with(&lines, "• act").starts_with('▶'),
        "the running node carries the marker"
    );
    assert_eq!(
        label_style(&terminal, "act").bg,
        Some(Color::Cyan),
        "and its label is highlighted"
    );
    assert!(!row_with(&lines, "• finalize").contains('▶'));

    drop(first);
    let _second = feed.enter("finalize", "finalize");
    let terminal = draw(&mut panel, 80, 24);
    let lines = text_of(terminal.backend().buffer());
    assert!(
        row_with(&lines, "• finalize").starts_with('▶'),
        "the marker follows the next node"
    );
    assert!(
        row_with(&lines, "• act").starts_with('✓'),
        "the node that ran is marked as having run"
    );
    assert_ne!(
        label_style(&terminal, "act").bg,
        Some(Color::Cyan),
        "and is no longer highlighted"
    );
    assert_eq!(
        label_style(&terminal, "finalize").bg,
        Some(Color::Cyan),
        "while the new one is"
    );
    assert!(
        row_with(&lines, "• prepare_context").starts_with(' '),
        "untouched nodes stay blank"
    );
}

#[test]
fn a_node_that_holds_the_running_node_is_marked_apart_from_it() {
    let (mut panel, feed) = panel_over(agent_like());
    let _run = feed.begin_run();
    let _outer = feed.enter("agent_loop", "agent_loop");
    let _inner = feed.enter("act", "act");
    let lines = screen(&mut panel, 80, 24);
    assert!(row_with(&lines, "agent_loop").starts_with('▸'));
    assert!(row_with(&lines, "• act").starts_with('▶'));
}

#[test]
fn running_time_and_repeat_counts_are_shown() {
    let (mut panel, feed) = panel_over(agent_like());
    let _run = feed.begin_run();
    let running = feed.enter("act", "act");
    let lines = screen(&mut panel, 80, 24);
    let row = row_with(&lines, "• act");
    // How long it has been is real elapsed time; the exact text is checked by
    // `elapsed_time_is_formatted_for_reading`.
    assert!(
        row.contains("running ") && row.ends_with('s'),
        "a running node shows how long it has run: {row:?}"
    );
    drop(running);
    drop(feed.enter("act", "act"));
    let lines = screen(&mut panel, 80, 24);
    let row = row_with(&lines, "• act");
    assert!(row.contains("×2"), "a node that ran twice says so: {row:?}");
}

#[test]
fn elapsed_time_is_formatted_for_reading() {
    assert_eq!(format_elapsed(Duration::from_millis(400)), "0.4s");
    assert_eq!(format_elapsed(Duration::from_millis(12_340)), "12.3s");
    assert_eq!(format_elapsed(Duration::from_secs(185)), "3m05s");
}

#[test]
fn a_topology_graph_replaces_the_default_one_for_its_run() {
    let (mut panel, feed) = panel_over(agent_like());
    assert!(screen(&mut panel, 80, 24)[0].contains("CodeAgent"));

    let topology = GraphModel::new(
        "prediction-error",
        vec![
            GraphNode::system("predict", "predict_edit"),
            GraphNode::system("verify", "verify_edit"),
        ],
    );
    let announcement = feed.announce(topology);
    let run = feed.begin_run();
    let node = feed.enter("predict", "predict_edit");
    let lines = screen(&mut panel, 80, 24);
    assert!(
        lines[0].contains("prediction-error"),
        "header: {:?}",
        lines[0]
    );
    assert!(row_with(&lines, "predict_edit").starts_with('▶'));
    assert!(
        lines.iter().all(|row| !row.contains("agent_loop")),
        "the default graph is gone while the topology runs:\n{}",
        lines.join("\n")
    );
    drop(node);
    drop(run);
    drop(announcement);

    let _next = feed.begin_run();
    let lines = screen(&mut panel, 80, 24);
    assert!(
        lines[0].contains("CodeAgent"),
        "the next run is the default graph again"
    );
    assert!(
        lines.iter().all(|row| !row.contains("predict_edit")),
        "{}",
        lines.join("\n")
    );
}

#[test]
fn an_empty_feed_says_what_the_tab_is_for() {
    let mut panel = GraphPanel::new(GraphFeed::new());
    let lines = screen(&mut panel, 80, 6);
    assert!(lines[0].contains("No graph yet"), "{lines:?}");
}

#[test]
fn a_graph_longer_than_the_terminal_scrolls() {
    let (mut panel, _feed) = panel_over(long_chain(40));
    // Ten rows: one header and nine of graph.
    let terminal = draw(&mut panel, 60, 10);
    let top = text_of(terminal.backend().buffer());
    assert!(
        top[0].contains("1-9/40"),
        "header shows the window: {:?}",
        top[0]
    );
    assert!(top[1].contains("s00") && top[9].contains("s08"));
    let buffer = terminal.backend().buffer();
    assert!(
        (1..10).all(|y| matches!(buffer[(59, y)].symbol(), "█" | "║")),
        "an overflowing graph draws a scrollbar in the last column"
    );

    panel.handle_event(&press(KeyCode::Down));
    let lines = screen(&mut panel, 60, 10);
    assert!(lines[0].contains("2-10/40") && lines[1].contains("s01"));

    panel.handle_event(&press(KeyCode::PageDown));
    let lines = screen(&mut panel, 60, 10);
    assert!(
        lines[1].contains("s09"),
        "a page is one window less a row: {lines:?}"
    );

    panel.handle_event(&press(KeyCode::End));
    let lines = screen(&mut panel, 60, 10);
    assert!(lines[0].contains("32-40/40") && lines[9].contains("s39"));

    panel.handle_event(&press(KeyCode::Home));
    assert!(screen(&mut panel, 60, 10)[1].contains("s00"));

    panel.handle_event(&wheel(MouseEventKind::ScrollDown));
    assert!(
        screen(&mut panel, 60, 10)[1].contains("s03"),
        "a wheel notch is three rows"
    );
    panel.handle_event(&wheel(MouseEventKind::ScrollUp));
    assert!(screen(&mut panel, 60, 10)[1].contains("s00"));
}

#[test]
fn a_graph_that_fits_draws_no_scrollbar() {
    let (mut panel, _feed) = panel_over(long_chain(3));
    let terminal = draw(&mut panel, 40, 10);
    let buffer = terminal.backend().buffer();
    assert!((1..10).all(|y| buffer[(39, y)].symbol() == " "));
}

#[test]
fn scrolling_never_runs_past_either_end() {
    let (mut panel, _feed) = panel_over(long_chain(12));
    screen(&mut panel, 40, 6);
    panel.handle_event(&press(KeyCode::Up));
    assert!(
        screen(&mut panel, 40, 6)[1].contains("s00"),
        "stays at the top"
    );
    for _ in 0..40 {
        panel.handle_event(&press(KeyCode::Down));
    }
    let lines = screen(&mut panel, 40, 6);
    assert!(lines[5].contains("s11"), "stops at the last row: {lines:?}");
}

#[test]
fn following_keeps_the_running_node_in_view_until_the_user_scrolls() {
    let (mut panel, feed) = panel_over(long_chain(40));
    let _run = feed.begin_run();
    screen(&mut panel, 60, 10);

    let node = feed.enter("s30", "s30");
    let lines = screen(&mut panel, 60, 10);
    assert!(
        row_with(&lines, "s30").starts_with('▶'),
        "the running node scrolled into view: {lines:?}"
    );
    assert!(lines[0].contains("following"));
    drop(node);

    // The user scrolls away; the panel must stop chasing the node.
    panel.handle_event(&press(KeyCode::Home));
    let _later = feed.enter("s36", "s36");
    let lines = screen(&mut panel, 60, 10);
    assert!(
        lines[1].contains("s00"),
        "the user's position is kept: {lines:?}"
    );
    assert!(lines[0].contains("f follow"));

    // `f` hands control back.
    panel.handle_event(&press(KeyCode::Char('f')));
    let lines = screen(&mut panel, 60, 10);
    assert!(row_with(&lines, "s36").starts_with('▶'), "{lines:?}");
}

#[test]
fn long_labels_are_cut_to_the_width_and_keep_the_running_time() {
    let label = "an_extremely_long_node_name_that_cannot_fit";
    let model = GraphModel::new("wide", vec![GraphNode::system("k", label)]);
    let (mut panel, feed) = panel_over(model);
    let _run = feed.begin_run();
    let _node = feed.enter("k", label);
    let lines = screen(&mut panel, 32, 4);
    let row = &lines[1];
    assert!(row.chars().count() <= 32, "{row:?}");
    assert!(row.contains('…'), "the label says it was cut: {row:?}");
    assert!(
        row.contains("running"),
        "the time survives the cut: {row:?}"
    );
}

#[test]
fn a_change_of_activity_does_not_rebuild_the_layout() {
    let (mut panel, feed) = panel_over(agent_like());
    screen(&mut panel, 80, 24);
    let rows_before = panel.rows.as_ptr();
    let generation = panel.rows_generation;

    let _run = feed.begin_run();
    let _node = feed.enter("act", "act");
    panel.tick();
    assert_eq!(panel.rows.as_ptr(), rows_before, "the rows are reused");
    assert_eq!(panel.rows_generation, generation);
    assert!(
        panel.snapshot.is_running("act"),
        "and the activity is current"
    );
}

#[test]
fn many_writes_between_frames_cost_one_read_and_no_layout() {
    let (mut panel, feed) = panel_over(agent_like());
    screen(&mut panel, 80, 24);
    let rows_before = panel.rows.as_ptr();
    let generation = panel.rows_generation;
    let _run = feed.begin_run();
    panel.tick();
    let seen_before = panel.seen_version;

    // A producer that reports a node thousands of times between two frames.
    for _ in 0..10_000 {
        drop(feed.enter("act", "act"));
    }
    assert_eq!(
        panel.seen_version, seen_before,
        "the producer never touches the panel; it only writes the feed"
    );

    // One tick reads the latest state once, and nothing is re-laid out.
    panel.tick();
    assert_eq!(panel.seen_version, Some(feed.version()));
    assert_eq!(panel.rows.as_ptr(), rows_before);
    assert_eq!(panel.rows_generation, generation);
    let lines = screen(&mut panel, 80, 24);
    assert!(
        row_with(&lines, "• act").contains("×10000"),
        "only the final state is drawn, however many writes led to it"
    );
}

#[test]
fn a_tick_with_nothing_new_takes_no_snapshot() {
    let (mut panel, feed) = panel_over(agent_like());
    panel.tick();
    let seen = panel.seen_version;
    panel.tick();
    assert_eq!(panel.seen_version, seen);
    assert_eq!(seen, Some(feed.version()));
}

#[test]
fn chords_with_ctrl_or_alt_held_are_not_scroll_keys() {
    let (mut panel, _feed) = panel_over(long_chain(40));
    screen(&mut panel, 60, 10);
    for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
        for code in [KeyCode::Char('j'), KeyCode::Char('f'), KeyCode::Down] {
            let chord = TuiEvent::Key(KeyEvent {
                code,
                modifiers: modifier,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            assert_eq!(
                panel.handle_event(&chord),
                EventOutcome::Continue,
                "{code:?} with {modifier:?} belongs to someone else"
            );
        }
    }
    assert!(
        screen(&mut panel, 60, 10)[1].contains("s00"),
        "and nothing scrolled"
    );
}

#[test]
fn a_tiny_terminal_does_not_panic() {
    let (mut panel, feed) = panel_over(agent_like());
    let _run = feed.begin_run();
    let _node = feed.enter("act", "act");
    for (width, height) in [(1, 1), (2, 1), (1, 5), (3, 2), (10, 1), (12, 3)] {
        screen(&mut panel, width, height);
    }
    panel.handle_event(&press(KeyCode::End));
    screen(&mut panel, 5, 2);
}

#[test]
fn keys_the_panel_does_not_use_fall_through_to_the_shell() {
    let (mut panel, _feed) = panel_over(agent_like());
    let ctrl_c = TuiEvent::Key(KeyEvent {
        code: KeyCode::Char('c'),
        modifiers: KeyModifiers::CONTROL,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    });
    assert_eq!(panel.handle_event(&ctrl_c), EventOutcome::Continue);
    assert_eq!(
        panel.handle_event(&press(KeyCode::Tab)),
        EventOutcome::Continue
    );
    assert_eq!(
        panel.handle_event(&press(KeyCode::Down)),
        EventOutcome::Consumed
    );
}

/// Stands in for the shell plugin, which owns the registry panels join.
struct ShellStandIn;

impl Plugin for ShellStandIn {
    const ID: &'static str = "test::shell";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server.insert_api(TuiRegistry::new());
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new().provides::<TuiRegistry>(TuiRegistry::CONTRACT_VERSION)
    }
}

#[tokio::test]
async fn the_plugin_contributes_the_panel_and_publishes_its_feed() {
    let feed = GraphFeed::new();
    let mut server = Server::new();
    server
        .add_plugins(ShellStandIn)
        .add_plugins(GraphPanelPlugin::with_feed(feed.clone()));
    server.finish().await.expect("the server assembles");

    let panels = server.api::<TuiRegistry>().expect("registry").drain();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].id(), "graph");
    assert_eq!(panels[0].title(), "Graph");

    // The global is the same feed, so a producer that reads it reaches the panel.
    let published = server
        .get_global::<GraphFeed>()
        .expect("the feed is published");
    published.publish(long_chain(3));
    assert_eq!(feed.snapshot().model().map(|m| m.name()), Some("long"));
}

#[tokio::test]
async fn the_plugin_is_built_after_the_shell_whichever_is_added_first() {
    let feed = GraphFeed::new();
    let mut server = Server::new();
    // The panel plugin is added ahead of the plugin that provides the registry
    // it requires: the declared requirement, not the order of the calls,
    // decides which is built first.
    server
        .add_plugins(GraphPanelPlugin::with_feed(feed.clone()))
        .add_plugins(ShellStandIn);
    server.finish().await.expect("the server assembles");

    let panels = server.api::<TuiRegistry>().expect("registry").drain();
    assert_eq!(panels.len(), 1);
    assert_eq!(panels[0].id(), "graph");

    server
        .get_global::<GraphFeed>()
        .expect("the feed is published")
        .publish(long_chain(3));
    assert_eq!(feed.snapshot().model().map(|m| m.name()), Some("long"));
}
