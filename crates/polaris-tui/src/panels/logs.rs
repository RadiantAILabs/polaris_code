//! Tracing-backed log panel.
//!
//! [`LogPanelPlugin`] installs a custom [`tracing_subscriber::Layer`] in its
//! `build()` phase that pushes formatted events into a ring buffer shared
//! with a [`LogPanel`]. The layer is installed as the **global default**
//! subscriber, which means this plugin cannot coexist with another plugin
//! that also takes the global slot (e.g.
//! [`polaris::plugins::TracingPlugin`](https://docs.rs/polaris-ai/latest/polaris_ai/plugins/index.html)).
//! Pick one.
//!
//! Custom subscriber stacks can use [`LogPanelPlugin::layer_only`] to take
//! just the panel and layer and compose them into a `tracing_subscriber`
//! `Registry` themselves.

use crate::event::TuiEvent;
use crate::panel::{EventOutcome, TuiPanel};
use crate::registry::TuiRegistry;
use crossterm::event::{KeyCode, MouseEventKind};
use parking_lot::Mutex;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Arc;
use tracing::span::{Attributes, Id};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, field::Visit};

const DEFAULT_CAPACITY: usize = 2_000;

/// Target prefix identifying events emitted from within the Polaris framework
/// itself (`polaris_*` crate module paths and explicit `polaris::*` targets).
/// Shared so the conversation panel's node-timeline filter and any
/// [`with_target_prefixes`](LogBufferLayer::with_target_prefixes) caller wanting
/// "framework only" reference one definition rather than re-spelling the string.
pub(crate) const FRAMEWORK_TARGET_PREFIX: &str = "polaris";

/// Rows moved per mouse-wheel notch, matching the conversation panel.
const WHEEL_LINES: usize = 3;

/// Span field carrying the graph node/system name. Emitted by a graph-execution
/// instrumentation middleware (e.g. `polaris_code_core`'s `ProgressMiddlewarePlugin`, or the
/// framework's `TracingPlugin`); read here to attribute each event to the node
/// whose span encloses it.
const NODE_SPAN_FIELD: &str = "polaris.graph.system.name";
/// Span field carrying a per-turn sequence index, disambiguating repeated
/// invocations of the same node across loop iterations. Optional.
const SEQ_SPAN_FIELD: &str = "polaris.graph.system.seq";

/// Severity of a log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// Trace.
    Trace,
    /// Debug.
    Debug,
    /// Info.
    Info,
    /// Warn.
    Warn,
    /// Error.
    Error,
}

impl From<&tracing::Level> for LogLevel {
    fn from(level: &tracing::Level) -> Self {
        match *level {
            tracing::Level::TRACE => Self::Trace,
            tracing::Level::DEBUG => Self::Debug,
            tracing::Level::INFO => Self::Info,
            tracing::Level::WARN => Self::Warn,
            tracing::Level::ERROR => Self::Error,
        }
    }
}

impl LogLevel {
    /// Display color for this severity.
    #[must_use]
    pub fn color(self) -> Color {
        match self {
            Self::Trace => Color::DarkGray,
            Self::Debug => Color::Gray,
            Self::Info => Color::Cyan,
            Self::Warn => Color::Yellow,
            Self::Error => Color::Red,
        }
    }

    /// Five-character padded severity label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO ",
            Self::Warn => "WARN ",
            Self::Error => "ERROR",
        }
    }
}

/// One log entry as buffered by the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Severity.
    pub level: LogLevel,
    /// Target (module path or explicit `target =` field).
    pub target: String,
    /// Formatted message + fields.
    pub message: String,
    /// Name of the graph node/system whose span enclosed this event, if any.
    /// Lets a consumer (e.g. the conversation panel's node timeline) bucket
    /// events by the node that produced them. `None` for events emitted
    /// outside any instrumented node span.
    pub node: Option<String>,
    /// Per-turn sequence index of the enclosing node span, disambiguating
    /// repeated invocations of the same node across loop iterations.
    pub seq: Option<u64>,
}

/// Bounded ring buffer of log entries, cheaply cloneable.
#[derive(Debug, Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<LogEntry>>>,
    capacity: usize,
}

impl LogBuffer {
    /// New buffer with the given capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if full.
    ///
    /// `capacity` is clamped to at least 1 so a `with_capacity(0)` buffer
    /// keeps the latest entry rather than growing without bound.
    pub fn push(&self, entry: LogEntry) {
        let mut buf = self.inner.lock();
        if buf.len() >= self.capacity.max(1) {
            buf.pop_front();
        }
        buf.push_back(entry);
    }

    /// Snapshot of current entries (cloned).
    #[must_use]
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.inner.lock().iter().cloned().collect()
    }

    /// Number of entries currently buffered.
    ///
    /// `O(1)` (a lock + length read), unlike [`snapshot`](Self::snapshot) which
    /// clones every entry — use this when you only need a cheap change signal.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Configured ring-buffer capacity.
    ///
    /// This is the value passed to [`with_capacity`](Self::with_capacity); the
    /// *effective* capacity [`push`](Self::push) enforces is `capacity.max(1)`,
    /// so a buffer built with `0` still retains the latest entry.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Node attribution captured from a graph-system span and stashed in the span's
/// extensions, so each event nested under it can be tagged with the node that
/// produced it.
#[derive(Clone)]
struct NodeContext {
    node: String,
    seq: Option<u64>,
}

/// Visitor that pulls [`NODE_SPAN_FIELD`] / [`SEQ_SPAN_FIELD`] off a span's
/// fields when the span is created.
#[derive(Default)]
struct SpanNodeVisitor {
    node: Option<String>,
    seq: Option<u64>,
}

impl Visit for SpanNodeVisitor {
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        if field.name() == SEQ_SPAN_FIELD {
            self.seq = Some(value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == NODE_SPAN_FIELD {
            self.node = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // Fallback for a node name not recorded as a plain `&str`.
        if field.name() == NODE_SPAN_FIELD && self.node.is_none() {
            self.node = Some(format!("{value:?}").trim_matches('"').to_string());
        }
    }
}

/// A [`Layer`] that pushes each event into a [`LogBuffer`].
///
/// An optional set of target prefixes filters which events are buffered: when
/// non-empty, only events whose `target` starts with one of the prefixes are
/// kept (e.g. `["polaris"]` drops noise from transitive deps like `hyper` or
/// `oxigraph`). Empty means buffer everything.
#[derive(Debug)]
pub struct LogBufferLayer {
    buffer: LogBuffer,
    target_prefixes: Arc<[String]>,
}

impl LogBufferLayer {
    /// Wrap a buffer as a tracing layer that buffers every event.
    #[must_use]
    pub fn new(buffer: LogBuffer) -> Self {
        Self {
            buffer,
            target_prefixes: Arc::from([]),
        }
    }

    /// Restrict buffering to events whose `target` starts with one of the given
    /// prefixes. Passing an empty iterator restores the buffer-everything
    /// default.
    #[must_use]
    pub fn with_target_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.target_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    /// Whether `target` passes the prefix filter (always true when no prefixes
    /// are configured).
    fn target_allowed(&self, target: &str) -> bool {
        self.target_prefixes.is_empty()
            || self
                .target_prefixes
                .iter()
                .any(|prefix| target.starts_with(prefix.as_str()))
    }
}

impl<S> Layer<S> for LogBufferLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        // Stash node attribution on the span so events nested under it can be
        // bucketed by node. Only spans that carry the node field qualify.
        let mut visitor = SpanNodeVisitor::default();
        attrs.record(&mut visitor);
        if let Some(node) = visitor.node
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(NodeContext {
                node,
                seq: visitor.seq,
            });
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let metadata = event.metadata();
        if !self.target_allowed(metadata.target()) {
            return;
        }
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        let (node, seq) = node_attribution(event, &ctx);
        self.buffer.push(LogEntry {
            level: metadata.level().into(),
            target: metadata.target().to_string(),
            message: visitor.message,
            node,
            seq,
        });
    }
}

/// Walk the event's span scope (nearest enclosing span outward to the root) and
/// return the first [`NodeContext`] found — i.e. the innermost node span around
/// the event. `(None, None)` when the event fired outside any node span.
fn node_attribution<S>(event: &Event<'_>, ctx: &Context<'_, S>) -> (Option<String>, Option<u64>)
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    if let Some(scope) = ctx.event_scope(event) {
        for span in scope {
            if let Some(node_ctx) = span.extensions().get::<NodeContext>() {
                return (Some(node_ctx.node.clone()), node_ctx.seq);
            }
        }
    }
    (None, None)
}

#[derive(Default)]
struct FieldVisitor {
    message: String,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            let _ = write!(&mut self.message, "{value:?}");
        } else {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            let _ = write!(&mut self.message, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            self.message.push_str(value);
        } else {
            if !self.message.is_empty() {
                self.message.push(' ');
            }
            let _ = write!(&mut self.message, "{}={value}", field.name());
        }
    }
}

/// Panel that displays the tail of a [`LogBuffer`].
pub struct LogPanel {
    buffer: LogBuffer,
    state: ListState,
    follow_tail: bool,
}

impl LogPanel {
    /// Construct a panel reading from `buffer`.
    #[must_use]
    pub fn new(buffer: LogBuffer) -> Self {
        Self {
            buffer,
            state: ListState::default(),
            follow_tail: true,
        }
    }

    fn items(&self) -> Vec<ListItem<'static>> {
        // Terminal escape/control chars in a log message are neutralised
        // downstream: ratatui-core filters them in `Span::styled_graphemes` /
        // `Buffer::set_stringn`, so raw entries can't inject cursor moves or
        // colour codes into the alternate screen. Don't drop that filter.
        self.buffer
            .snapshot()
            .into_iter()
            .map(|entry| {
                let level_color = entry.level.color();
                let line = Line::from(vec![
                    Span::styled(entry.level.label(), Style::default().fg(level_color)),
                    Span::raw(" "),
                    Span::styled(entry.target, Style::default().fg(Color::DarkGray)),
                    Span::raw(" "),
                    Span::raw(entry.message),
                ]);
                ListItem::new(line)
            })
            .collect()
    }
}

impl TuiPanel for LogPanel {
    fn id(&self) -> &str {
        "logs"
    }
    fn title(&self) -> &str {
        "Logs"
    }

    fn tick(&mut self) {}

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        let len = self.buffer.len();
        // Mouse-wheel scroll: step the selection like Up/Down. Only arrives
        // when the host enables mouse capture; a no-op otherwise.
        if let TuiEvent::Mouse(mouse) = event {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.follow_tail = false;
                    let i = self
                        .state
                        .selected()
                        .unwrap_or(len)
                        .saturating_sub(WHEEL_LINES);
                    self.state.select(Some(i));
                    return EventOutcome::Consumed;
                }
                MouseEventKind::ScrollDown => {
                    let cur = self.state.selected().unwrap_or(0);
                    let next = (cur + WHEEL_LINES).min(len.saturating_sub(1));
                    if next + 1 == len {
                        self.follow_tail = true;
                    }
                    self.state.select(Some(next));
                    return EventOutcome::Consumed;
                }
                _ => return EventOutcome::Continue,
            }
        }
        let TuiEvent::Key(key) = event else {
            return EventOutcome::Continue;
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.follow_tail = false;
                let i = self.state.selected().unwrap_or(len).saturating_sub(1);
                self.state.select(Some(i));
                EventOutcome::Consumed
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let cur = self.state.selected().unwrap_or(0);
                let next = (cur + 1).min(len.saturating_sub(1));
                if next + 1 == len {
                    self.follow_tail = true;
                }
                self.state.select(Some(next));
                EventOutcome::Consumed
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.follow_tail = true;
                self.state.select(len.checked_sub(1));
                EventOutcome::Consumed
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.follow_tail = false;
                self.state.select(Some(0));
                EventOutcome::Consumed
            }
            _ => EventOutcome::Continue,
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let items = self.items();
        let len = items.len();
        if self.follow_tail && len > 0 {
            self.state.select(Some(len - 1));
        }
        let list =
            List::new(items).highlight_style(Style::default().fg(Color::Black).bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}

/// Plugin that installs a [`LogBufferLayer`] and contributes a [`LogPanel`].
///
/// `build()` installs the layer as the global tracing default. If a global
/// default is already set, the install attempt logs a `tracing::warn!` (not
/// stderr, which would corrupt the alternate screen) and the panel will
/// simply stay empty.
pub struct LogPanelPlugin {
    capacity: usize,
    /// An externally supplied buffer to install the layer over and feed the
    /// panel. When `None`, the plugin creates its own buffer of `capacity`.
    /// Sharing a buffer lets another panel (e.g. the conversation timeline)
    /// read the same node-attributed entries.
    buffer: Option<LogBuffer>,
    /// Target prefixes the installed layer keeps; empty buffers everything.
    target_prefixes: Vec<String>,
}

impl Default for LogPanelPlugin {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
            buffer: None,
            target_prefixes: Vec::new(),
        }
    }
}

impl LogPanelPlugin {
    /// Construct the plugin with a custom ring-buffer capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            buffer: None,
            target_prefixes: Vec::new(),
        }
    }

    /// Construct the plugin over an externally owned [`LogBuffer`].
    ///
    /// The layer is installed against this buffer and the panel reads from it,
    /// so a caller can share the same buffer with another consumer (for
    /// example, the conversation panel's per-node timeline).
    #[must_use]
    pub fn with_buffer(buffer: LogBuffer) -> Self {
        Self {
            capacity: buffer.capacity(),
            buffer: Some(buffer),
            target_prefixes: Vec::new(),
        }
    }

    /// Restrict the installed layer to events whose `target` starts with one of
    /// the given prefixes (e.g. `["polaris"]` to drop transitive-dep noise).
    /// Empty (the default) buffers every event. Only affects what the layer
    /// captures — the panel and any shared buffer are unchanged.
    #[must_use]
    pub fn with_target_prefixes<I, S>(mut self, prefixes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.target_prefixes = prefixes.into_iter().map(Into::into).collect();
        self
    }

    /// Build just the buffer and panel without touching the global tracing
    /// subscriber. Use this if you want to compose [`LogBufferLayer`] into
    /// your own subscriber stack.
    #[must_use]
    pub fn layer_only(capacity: usize) -> (LogBuffer, LogBufferLayer, LogPanel) {
        let buffer = LogBuffer::with_capacity(capacity);
        let layer = LogBufferLayer::new(buffer.clone());
        let panel = LogPanel::new(buffer.clone());
        (buffer, layer, panel)
    }
}

impl Plugin for LogPanelPlugin {
    const ID: &'static str = "polaris::tui::log_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let buffer = self
            .buffer
            .clone()
            .unwrap_or_else(|| LogBuffer::with_capacity(self.capacity));
        let layer =
            LogBufferLayer::new(buffer.clone()).with_target_prefixes(self.target_prefixes.clone());
        let panel = LogPanel::new(buffer);
        let subscriber = tracing_subscriber::registry().with(layer);
        if let Err(err) = subscriber.try_init() {
            // Route through tracing rather than stderr: a TUI owns the
            // alternate screen, so a raw `eprintln!` would corrupt the
            // display. The already-installed global default captures this.
            tracing::warn!(
                error = %err,
                "polaris-tui: LogPanelPlugin could not install its tracing subscriber \
                 (another global default is already set); the Logs panel will stay empty"
            );
        }
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin must be added before LogPanelPlugin")
            .add_panel(panel);
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(message: &str) -> LogEntry {
        LogEntry {
            level: LogLevel::Info,
            target: "test".to_string(),
            message: message.to_string(),
            node: None,
            seq: None,
        }
    }

    #[test]
    fn ring_buffer_evicts_oldest_at_capacity() {
        let buf = LogBuffer::with_capacity(2);
        buf.push(entry("a"));
        buf.push(entry("b"));
        buf.push(entry("c"));
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].message, "b");
        assert_eq!(snap[1].message, "c");
    }

    #[test]
    fn zero_capacity_keeps_latest_not_unbounded() {
        // `with_capacity(0)` must keep only the latest entry: `len() == 0`
        // never matches the eviction guard, so the buffer would otherwise grow
        // without bound.
        let buf = LogBuffer::with_capacity(0);
        for i in 0..100 {
            buf.push(entry(&i.to_string()));
        }
        let snap = buf.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].message, "99");
    }

    #[test]
    fn events_are_attributed_to_the_enclosing_node_span() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = LogBuffer::with_capacity(16);
        let subscriber = tracing_subscriber::registry().with(LogBufferLayer::new(buffer.clone()));

        with_default(subscriber, || {
            // An event inside a node span is stamped with (node, seq).
            let span = tracing::info_span!(
                "polaris.graph.execute_system",
                polaris.graph.system.name = "act",
                polaris.graph.system.seq = 3u64,
            );
            span.in_scope(|| tracing::info!("inside act"));
            // An event outside any node span gets no attribution.
            tracing::info!("outside");
        });

        let snap = buffer.snapshot();
        let inside = snap.iter().find(|e| e.message == "inside act").unwrap();
        assert_eq!(inside.node.as_deref(), Some("act"));
        assert_eq!(inside.seq, Some(3));
        let outside = snap.iter().find(|e| e.message == "outside").unwrap();
        assert_eq!(outside.node, None);
        assert_eq!(outside.seq, None);
    }

    #[test]
    fn target_prefix_filter_keeps_only_matching_targets() {
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = LogBuffer::with_capacity(16);
        let layer = LogBufferLayer::new(buffer.clone()).with_target_prefixes(["polaris"]);
        let subscriber = tracing_subscriber::registry().with(layer);

        with_default(subscriber, || {
            tracing::info!(target: "polaris_code::agent", "kept");
            tracing::info!(target: "polaris_ontrace", "kept too");
            tracing::info!(target: "hyper::client", "dropped");
            tracing::info!(target: "oxigraph::store", "dropped too");
        });

        let snap = buffer.snapshot();
        let targets: Vec<&str> = snap.iter().map(|e| e.target.as_str()).collect();
        assert_eq!(snap.len(), 2, "only polaris* targets kept: {targets:?}");
        assert!(targets.iter().all(|t| t.starts_with("polaris")));
    }

    #[test]
    fn empty_target_prefixes_buffer_everything() {
        let layer = LogBufferLayer::new(LogBuffer::with_capacity(4));
        assert!(layer.target_allowed("hyper::client"));
        assert!(layer.target_allowed("polaris_code"));
        let filtered =
            LogBufferLayer::new(LogBuffer::with_capacity(4)).with_target_prefixes(["polaris"]);
        assert!(!filtered.target_allowed("hyper::client"));
        assert!(filtered.target_allowed("polaris_code"));
    }

    #[test]
    fn wheel_scroll_moves_the_selection_and_unfollows() {
        use crossterm::event::{KeyModifiers, MouseEvent};
        let buf = LogBuffer::with_capacity(16);
        for i in 0..10 {
            buf.push(entry(&i.to_string()));
        }
        let mut panel = LogPanel::new(buf);
        let wheel = |kind| {
            TuiEvent::Mouse(MouseEvent {
                kind,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        // Start at the tail (index 9), scroll up a notch (3 rows) → index 6.
        panel.state.select(Some(9));
        assert!(matches!(
            panel.handle_event(&wheel(MouseEventKind::ScrollUp)),
            EventOutcome::Consumed
        ));
        assert_eq!(panel.state.selected(), Some(6));
        assert!(!panel.follow_tail, "scrolling up stops following the tail");
        // Scroll back down past the end clamps to the last row and re-follows.
        panel.handle_event(&wheel(MouseEventKind::ScrollDown));
        panel.handle_event(&wheel(MouseEventKind::ScrollDown));
        assert_eq!(panel.state.selected(), Some(9));
        assert!(panel.follow_tail);
    }

    #[test]
    fn node_attributed_via_record_debug_fallback() {
        // When the node name isn't recorded as a plain `&str` (e.g. emitted via
        // a `Debug`-valued field), `record_debug` must still attribute the event,
        // stripping the surrounding quotes `{:?}` adds to a string.
        use tracing::subscriber::with_default;
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = LogBuffer::with_capacity(16);
        let subscriber = tracing_subscriber::registry().with(LogBufferLayer::new(buffer.clone()));

        with_default(subscriber, || {
            // `?value` forces the field through `record_debug`, so the recorded
            // value is `"act"` (with quotes) rather than a plain `act`.
            let node = "act";
            let span = tracing::info_span!(
                "polaris.graph.execute_system",
                polaris.graph.system.name = ?node,
            );
            span.in_scope(|| tracing::info!("inside act"));
        });

        let snap = buffer.snapshot();
        let inside = snap.iter().find(|e| e.message == "inside act").unwrap();
        assert_eq!(
            inside.node.as_deref(),
            Some("act"),
            "debug-recorded node name has its quotes stripped"
        );
    }

    #[test]
    fn keyboard_nav_steps_and_jumps_the_selection() {
        use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        let buf = LogBuffer::with_capacity(16);
        for i in 0..10 {
            buf.push(entry(&i.to_string()));
        }
        let mut panel = LogPanel::new(buf);
        let press = |code| {
            TuiEvent::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            })
        };

        // `G` / End jumps to the tail and re-follows.
        panel.handle_event(&press(KeyCode::Char('G')));
        assert_eq!(panel.state.selected(), Some(9));
        assert!(panel.follow_tail);

        // `k` / Up steps back one and stops following.
        panel.handle_event(&press(KeyCode::Char('k')));
        assert_eq!(panel.state.selected(), Some(8));
        assert!(!panel.follow_tail);

        // `g` / Home jumps to the top, still unfollowed.
        panel.handle_event(&press(KeyCode::Char('g')));
        assert_eq!(panel.state.selected(), Some(0));
        assert!(!panel.follow_tail);

        // `j` / Down steps forward one.
        panel.handle_event(&press(KeyCode::Down));
        assert_eq!(panel.state.selected(), Some(1));

        // Stepping down to the last row re-engages tail-following.
        panel.handle_event(&press(KeyCode::End));
        panel.handle_event(&press(KeyCode::Up));
        assert!(!panel.follow_tail);
        panel.handle_event(&press(KeyCode::Char('j')));
        assert_eq!(panel.state.selected(), Some(9));
        assert!(panel.follow_tail);
    }

    #[test]
    fn level_maps_from_tracing_level() {
        assert!(matches!(
            LogLevel::from(&tracing::Level::TRACE),
            LogLevel::Trace
        ));
        assert!(matches!(
            LogLevel::from(&tracing::Level::ERROR),
            LogLevel::Error
        ));
    }
}
