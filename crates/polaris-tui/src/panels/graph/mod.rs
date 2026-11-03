//! Live graph panel: draws the graph a host is running and lights up the node
//! that is executing.
//!
//! The panel is a pure renderer. It draws a [`GraphModel`] — a chain of steps
//! with labelled branches — and overlays what a [`GraphFeed`] says is running
//! or has run. It does not depend on any graph engine: the host converts its
//! own graph into the model and reports activity against the model's keys.
//!
//! # Layout
//!
//! The graph is drawn as an outline, one node per row, so a large graph fits
//! in a small terminal and scrolls instead of sprawling:
//!
//! ```text
//!  CodeAgent · 8 nodes · run 1 · running      · following
//! ✓ • receive_user_input                              0.0s
//! ✓ • init_loop                                       0.0s
//! ▸ ↻ agent_loop  repeats                     running 4.2s
//! ✓   ├─ • prepare_context                            0.1s
//! ▶   ├─ • act                                running 2.1s
//!     └─ ? has_tool_calls
//!        ├─ yes → ↻ tool_calls
//!        └─ no → • finalize
//! ```
//!
//! A sequence reads top to bottom and a connector joins each step to the one
//! after it. A choice, loop, parallel group or scope draws its branches nested
//! beneath it, and a branch that is a single step shares its row with its
//! label. The marker at the left shows state: `▶` a node running, `▸` a node
//! that is running something inside it, `✓` a node that has run in this run, a
//! blank for one that has not. The header names the graph and counts its nodes;
//! at its right edge it shows the visible rows when the graph scrolls and
//! whether the panel is following the running node.
//!
//! # Keys
//!
//! | Key | Action |
//! |-----|--------|
//! | `↑` / `k`, `↓` / `j` | Scroll one row. |
//! | `PageUp` / `PageDown` | Scroll one page. |
//! | `Home` / `g`, `End` / `G` | Jump to the top or bottom. |
//! | `f` | Follow the running node again. |
//! | Mouse wheel | Scroll three rows. |
//!
//! Scrolling stops following; the panel scrolls itself only while following.

mod feed;
mod model;

pub use feed::{Announcement, GraphFeed, GraphSnapshot, NodeRun, NodeSpan, RunSpan, RunningNode};
pub use model::{GraphBranch, GraphModel, GraphNode, GraphNodeKind};

use crate::event::TuiEvent;
use crate::panel::{EventOutcome, TuiPanel};
use crate::registry::TuiRegistry;
use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Rows moved per mouse-wheel notch.
const WHEEL_ROWS: usize = 3;
/// Columns a nested level indents by.
const INDENT: &str = "   ";
/// The guide drawn beside a chain that continues below.
const GUIDE: &str = "│  ";

/// What a drawn row stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowKind {
    /// A node, identified by its key.
    Node {
        key: String,
        kind: GraphNodeKind,
        note: Option<String>,
    },
    /// A branch label with steps nested beneath it.
    Branch,
}

/// One drawn row: its guides, the branch label that leads into it (if any) and
/// what it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    guides: String,
    lead: Option<String>,
    text: String,
    kind: RowKind,
}

/// Flattens a model into rows, once per model.
fn flatten(model: &GraphModel) -> Vec<Row> {
    let mut rows = Vec::new();
    emit_chain(model.steps(), "", true, &mut rows);
    for branch in model.side_branches() {
        if !branch.label().is_empty() {
            rows.push(Row {
                guides: String::new(),
                lead: None,
                text: branch.label().to_string(),
                kind: RowKind::Branch,
            });
        }
        emit_chain(branch.steps(), "  ", false, &mut rows);
    }
    rows
}

/// Draws a chain of steps. The first level of a graph carries no connector;
/// nested levels do.
fn emit_chain(steps: &[GraphNode], indent: &str, top: bool, rows: &mut Vec<Row>) {
    for (index, step) in steps.iter().enumerate() {
        let last = index + 1 == steps.len();
        let connector = match (top, last) {
            (true, _) => "",
            (false, true) => "└─ ",
            (false, false) => "├─ ",
        };
        rows.push(node_row(format!("{indent}{connector}"), None, step));
        let below = if top {
            format!("{indent}  ")
        } else if last {
            format!("{indent}{INDENT}")
        } else {
            format!("{indent}{GUIDE}")
        };
        emit_branches(step.branches(), &below, rows);
    }
}

fn emit_branches(branches: &[GraphBranch], indent: &str, rows: &mut Vec<Row>) {
    for (index, branch) in branches.iter().enumerate() {
        emit_branch(branch, indent, index + 1 == branches.len(), rows);
    }
}

/// Draws one branch: nothing for an unnamed one, a single row for a named
/// branch of one plain step, and a label row with its steps beneath otherwise.
fn emit_branch(branch: &GraphBranch, indent: &str, last: bool, rows: &mut Vec<Row>) {
    if branch.label().is_empty() {
        emit_chain(branch.steps(), indent, false, rows);
        return;
    }
    let connector = if last { "└─ " } else { "├─ " };
    let guides = format!("{indent}{connector}");
    if let [only] = branch.steps()
        && only.branches().is_empty()
    {
        rows.push(node_row(guides, Some(branch.label().to_string()), only));
        return;
    }
    rows.push(Row {
        guides,
        lead: None,
        text: branch.label().to_string(),
        kind: RowKind::Branch,
    });
    let below = format!("{indent}{}", if last { INDENT } else { GUIDE });
    emit_chain(branch.steps(), &below, false, rows);
}

fn node_row(guides: String, lead: Option<String>, node: &GraphNode) -> Row {
    Row {
        guides,
        lead,
        text: node.label().to_string(),
        kind: RowKind::Node {
            key: node.key().to_string(),
            kind: node.kind(),
            note: node.note().map(str::to_string),
        },
    }
}

/// Shortens `text` to at most `width` cells, ending in `…` when it was cut.
fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.to_string().width();
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    if width > 0 {
        out.push('…');
    }
    out
}

/// Formats a duration the way the panel shows it: `0.4s`, `12.3s`, `3m05s`.
fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let whole = elapsed.as_secs();
        format!("{}m{:02}s", whole / 60, whole % 60)
    }
}

/// Where a node stands, for the marker and the status text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    Pending,
    Ran,
    Running,
}

/// Panel that draws a [`GraphModel`] and the activity a [`GraphFeed`] reports.
#[derive(Debug)]
pub struct GraphPanel {
    feed: GraphFeed,
    seen_version: Option<u64>,
    snapshot: GraphSnapshot,
    rows: Vec<Row>,
    row_of_key: HashMap<String, usize>,
    rows_generation: Option<u64>,
    /// The shown model's node count, kept with the rows it was counted for.
    node_count: usize,
    offset: usize,
    follow: bool,
    /// The visible row count at the last render; paging steps by it.
    page: usize,
}

impl GraphPanel {
    /// Creates a panel drawing what `feed` holds.
    #[must_use]
    pub fn new(feed: GraphFeed) -> Self {
        let snapshot = feed.snapshot();
        let mut panel = Self {
            feed,
            seen_version: None,
            snapshot,
            rows: Vec::new(),
            row_of_key: HashMap::new(),
            rows_generation: None,
            node_count: 0,
            offset: 0,
            follow: true,
            page: 10,
        };
        panel.refresh();
        panel
    }

    /// Takes a new snapshot if the feed changed since the last one.
    ///
    /// The structure is re-flattened only when the model itself was replaced;
    /// a change of activity alone reads a few fields and nothing more.
    fn refresh(&mut self) {
        let version = self.feed.version();
        if self.seen_version == Some(version) {
            return;
        }
        self.seen_version = Some(version);
        self.snapshot = self.feed.snapshot();
        if self.rows_generation != Some(self.snapshot.generation()) {
            self.rows_generation = Some(self.snapshot.generation());
            self.node_count = self.snapshot.model().map_or(0, |model| model.node_count());
            self.rows = self
                .snapshot
                .model()
                .map(|model| flatten(model))
                .unwrap_or_default();
            self.row_of_key.clear();
            for (index, row) in self.rows.iter().enumerate() {
                if let RowKind::Node { key, .. } = &row.kind {
                    self.row_of_key.entry(key.clone()).or_insert(index);
                }
            }
            self.offset = 0;
        }
    }

    fn standing(&self, key: &str) -> Standing {
        if self.snapshot.is_running(key) {
            Standing::Running
        } else if self.snapshot.finished(key).is_some() {
            Standing::Ran
        } else {
            Standing::Pending
        }
    }

    /// The row to keep in view while following: the lowest drawn node that is
    /// running, which is the innermost one when nodes nest.
    fn followed_row(&self) -> Option<usize> {
        self.snapshot
            .running()
            .iter()
            .rev()
            .filter_map(|node| self.row_of_key.get(node.key()).copied())
            .max()
    }

    fn max_offset(&self, visible: usize) -> usize {
        self.rows.len().saturating_sub(visible)
    }

    fn scroll_by(&mut self, delta: isize) {
        self.follow = false;
        let max = self.max_offset(self.page);
        self.offset = self.offset.saturating_add_signed(delta).min(max);
    }

    fn row_line(&self, row: &Row, width: usize, now: Instant) -> Line<'static> {
        let dim = Style::default().fg(Color::DarkGray);
        let (standing, kind, note, key) = match &row.kind {
            RowKind::Node { key, kind, note } => (self.standing(key), Some(*kind), note, Some(key)),
            RowKind::Branch => (Standing::Pending, None, &None, None),
        };
        let marker = match (standing, kind) {
            (Standing::Running, Some(kind)) if kind.is_container() => ("▸ ", Color::Cyan),
            (Standing::Running, _) => ("▶ ", Color::Cyan),
            (Standing::Ran, _) => ("✓ ", Color::Green),
            (Standing::Pending, _) => ("  ", Color::DarkGray),
        };
        let status = key.and_then(|key| match standing {
            Standing::Running => self.snapshot.running_since(key).map(|since| {
                format!(
                    "running {}",
                    format_elapsed(now.saturating_duration_since(since))
                )
            }),
            Standing::Ran => self.snapshot.finished(key).map(|run| {
                let took = format_elapsed(run.last);
                if run.count > 1 {
                    format!("×{} {took}", run.count)
                } else {
                    took
                }
            }),
            Standing::Pending => None,
        });
        let status_width = status.as_deref().map_or(0, |s| s.width() + 2);

        let label_style = match standing {
            Standing::Running if kind.is_some_and(|k| !k.is_container()) => Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Standing::Running => Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            Standing::Ran => Style::default(),
            Standing::Pending if kind.is_none() => dim,
            Standing::Pending => Style::default().fg(Color::Gray),
        };

        let lead = row
            .lead
            .as_deref()
            .map(|lead| format!("{lead} → "))
            .unwrap_or_default();
        let glyph = kind.map(|k| format!("{} ", k.glyph())).unwrap_or_default();
        let note_text = note
            .as_deref()
            .map(|n| format!("  {n}"))
            .unwrap_or_default();

        let fixed = marker.0.width() + row.guides.width() + lead.width() + glyph.width();
        let room = width.saturating_sub(fixed + status_width);
        let label = truncate(
            &row.text,
            room.saturating_sub(note_text.width().min(room / 2)),
        );
        let note_room = room.saturating_sub(label.width());
        let note_text = truncate(&note_text, note_room);

        let used = fixed + label.width() + note_text.width();
        let pad = width.saturating_sub(used + status_width);

        let mut spans = vec![
            Span::styled(marker.0, Style::default().fg(marker.1)),
            Span::styled(row.guides.clone(), dim),
            Span::styled(lead, dim),
            Span::styled(glyph, dim),
            Span::styled(label, label_style),
            Span::styled(note_text, dim),
        ];
        if let Some(status) = status {
            spans.push(Span::raw(" ".repeat(pad + 2)));
            let status_style = if standing == Standing::Running {
                Style::default().fg(Color::Cyan)
            } else {
                dim
            };
            spans.push(Span::styled(status, status_style));
        }
        Line::from(spans)
    }

    fn header(&self, width: usize, visible: usize) -> Line<'static> {
        let dim = Style::default().fg(Color::DarkGray);
        let Some(model) = self.snapshot.model() else {
            return Line::from(Span::styled(" Graph ", dim));
        };
        let state = if self.snapshot.running().is_empty() {
            "idle"
        } else {
            "running"
        };
        let mut text = format!(" {} · {} nodes", model.name(), self.node_count);
        if self.snapshot.run() > 0 {
            text.push_str(&format!(" · run {} · {state}", self.snapshot.run()));
        }
        let total = self.rows.len();
        let mut tail = String::new();
        if total > visible {
            let first = self.offset + 1;
            let last = (self.offset + visible).min(total);
            tail.push_str(&format!("{first}-{last}/{total}"));
        }
        tail.push_str(if self.follow {
            " · following "
        } else {
            " · f follow "
        });
        let room = width.saturating_sub(tail.width() + 1);
        let text = truncate(&text, room);
        let pad = width.saturating_sub(text.width() + tail.width());
        Line::from(vec![
            Span::styled(text, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(pad)),
            Span::styled(tail, dim),
        ])
    }
}

impl TuiPanel for GraphPanel {
    fn id(&self) -> &str {
        "graph"
    }

    fn title(&self) -> &str {
        "Graph"
    }

    fn tick(&mut self) {
        self.refresh();
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        let page = isize::try_from(self.page.saturating_sub(1).max(1)).unwrap_or(1);
        match event {
            TuiEvent::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll_by(-(WHEEL_ROWS as isize));
                    EventOutcome::Consumed
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_by(WHEEL_ROWS as isize);
                    EventOutcome::Consumed
                }
                _ => EventOutcome::Continue,
            },
            // A chord with Ctrl or Alt held is not a scroll key; leave it to
            // the shell and to the other panels' conventions.
            TuiEvent::Key(key)
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                EventOutcome::Continue
            }
            TuiEvent::Key(key) => match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll_by(-1);
                    EventOutcome::Consumed
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll_by(1);
                    EventOutcome::Consumed
                }
                KeyCode::PageUp => {
                    self.scroll_by(-page);
                    EventOutcome::Consumed
                }
                KeyCode::PageDown => {
                    self.scroll_by(page);
                    EventOutcome::Consumed
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.follow = false;
                    self.offset = 0;
                    EventOutcome::Consumed
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.follow = false;
                    self.offset = self.max_offset(self.page);
                    EventOutcome::Consumed
                }
                KeyCode::Char('f') => {
                    self.follow = true;
                    EventOutcome::Consumed
                }
                _ => EventOutcome::Continue,
            },
            _ => EventOutcome::Continue,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.refresh();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.rows.is_empty() {
            let hint = Paragraph::new(
                " No graph yet. The agent's graph appears here once it is assembled,\n \
                 and the running node lights up as each turn runs.",
            )
            .style(Style::default().fg(Color::DarkGray));
            frame.render_widget(hint, area);
            return;
        }

        let visible = usize::from(area.height.saturating_sub(1)).max(1);
        self.page = visible;
        if self.follow
            && let Some(row) = self.followed_row()
        {
            if row < self.offset {
                self.offset = row;
            } else if row >= self.offset + visible {
                self.offset = row + 1 - visible;
            }
        }
        self.offset = self.offset.min(self.max_offset(visible));

        let overflows = self.rows.len() > visible;
        let body_width = usize::from(area.width).saturating_sub(usize::from(overflows));
        let now = Instant::now();

        let header_area = Rect { height: 1, ..area };
        frame.render_widget(
            Paragraph::new(self.header(usize::from(area.width), visible)),
            header_area,
        );

        let lines: Vec<Line<'static>> = self
            .rows
            .iter()
            .skip(self.offset)
            .take(visible)
            .map(|row| self.row_line(row, body_width, now))
            .collect();
        let body = Rect {
            y: area.y + 1,
            height: area.height - 1,
            width: u16::try_from(body_width).unwrap_or(area.width),
            ..area
        };
        frame.render_widget(Paragraph::new(lines), body);

        if overflows {
            let track = Rect {
                x: area.x + area.width - 1,
                y: area.y + 1,
                width: 1,
                height: area.height - 1,
            };
            let mut state = ScrollbarState::new(self.max_offset(visible) + 1)
                .position(self.offset)
                .viewport_content_length(visible);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None),
                track,
                &mut state,
            );
        }
    }
}

/// Plugin that contributes a [`GraphPanel`] and publishes the [`GraphFeed`] it
/// draws. Use it when your host runs a graph and should show it live in a tab,
/// with the running node lit up.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | [`GraphFeed`] | Global | The graph being shown and what is running in it; producers write it, the panel reads it. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | The plugin's contract is the [`GraphFeed`] global above; it adds no build-time API. |
///
/// # Dependencies
///
/// Requires the [`TuiRegistry`] capability, which [`TuiPlugin`](crate::TuiPlugin)
/// provides. The resolver builds the provider first, whatever the order of the
/// `add_plugins` calls. `build()` panics if no plugin provides it.
///
/// # Panels Contributed
///
/// | Panel id | Tab title | Renders |
/// |----------|-----------|---------|
/// | `graph` | `Graph` | The [`GraphModel`] the feed holds as an outline, one node per row, marking the node that is running and the ones that have run, with how long each took. |
///
/// # Keybindings
///
/// | Chord | Action |
/// |-------|--------|
/// | `↑` / `k`, `↓` / `j` | Scroll one row. |
/// | `PageUp` / `PageDown` | Scroll one page. |
/// | `Home` / `g`, `End` / `G` | Jump to the top or bottom. |
/// | `f` | Follow the running node again. |
/// | Mouse wheel | Scroll three rows. |
///
/// Chords held with Ctrl or Alt are left to the shell. Scrolling stops the
/// panel following the running node.
///
/// # Lifecycle
///
/// `build()` inserts the feed as a global resource and adds the panel to the
/// [`TuiRegistry`]. A host that wants its own handle to the feed passes one to
/// [`with_feed`](Self::with_feed); a plugin that only produces activity reads
/// the global.
///
/// # Panics
///
/// `build()` panics if a [`GraphFeed`] global already exists, so only one
/// `GraphPanelPlugin` may be added to a server: a shell shows one graph. It
/// also panics if no plugin provides the [`TuiRegistry`].
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris_tui::panels::GraphPanelPlugin;
/// use polaris_tui::{TuiConfig, TuiPlugin};
///
/// # async fn run() {
/// let mut server = Server::new();
/// server
///     .add_plugins(TuiPlugin::new(TuiConfig::default()))
///     .add_plugins(GraphPanelPlugin::default());
/// server.run().await;
/// # }
/// ```
#[derive(Debug, Default)]
pub struct GraphPanelPlugin {
    feed: GraphFeed,
}

impl GraphPanelPlugin {
    /// Creates the plugin over a fresh feed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates the plugin over `feed`, which the caller keeps a clone of.
    #[must_use]
    pub fn with_feed(feed: GraphFeed) -> Self {
        Self { feed }
    }
}

impl Plugin for GraphPanelPlugin {
    const ID: &'static str = "polaris::tui::graph_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        assert!(
            server.insert_global(self.feed.clone()).is_none(),
            "a GraphFeed global already exists; a shell shows one graph"
        );
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added before GraphPanelPlugin")
            .add_panel(GraphPanel::new(self.feed.clone()));
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
            .provides::<GraphFeed>(GraphFeed::CONTRACT_VERSION)
    }
}

#[cfg(test)]
mod tests;
