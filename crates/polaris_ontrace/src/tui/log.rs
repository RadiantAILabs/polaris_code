//! Filtered log panel for the inspect TUI.
//!
//! Replacement for [`polaris_tui::panels::LogPanelPlugin`] that wraps the
//! same buffer-backed [`LogPanel`] with a
//! [`tracing_subscriber::filter::Targets`] filter — only events from
//! `polaris_ontrace`, `polaris_ai`, and adjacent polaris crates make it into
//! the panel. Third-party noise (HTTP clients, oxigraph internals, hyper)
//! gets dropped.

use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::panels::{LogPanel, LogPanelPlugin};
use polaris_tui::ratatui::Frame;
use polaris_tui::ratatui::layout::Rect;
use polaris_tui::{EventOutcome, TuiEvent, TuiPanel, TuiRegistry};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_CAPACITY: usize = 2_000;

/// A target-filtered log panel for the `ontrace inspect` polaris-tui shell.
///
/// Drop-in replacement for [`polaris_tui::panels::LogPanelPlugin`]: it wraps
/// the same buffer-backed [`LogPanel`] but installs a
/// [`tracing_subscriber::filter::Targets`] filter so only events from
/// `polaris_ontrace`, `polaris_ai`, and adjacent polaris crates reach the
/// panel — third-party noise (HTTP clients, oxigraph internals, hyper) is
/// dropped. The panel is further wrapped in a focus gate so its `j`/`k`/arrow
/// scroll shortcuts only fire when the panel actually has focus.
///
/// Use this instead of [`LogPanelPlugin`] when you want a quiet, polaris-scoped
/// log view in the inspect shell. Use [`with_capacity`](Self::with_capacity) to
/// size the ring buffer (default 2000 lines).
///
/// # Resources Provided
///
/// _None._ The plugin installs a process-global
/// [`tracing_subscriber`] default and registers a panel, but inserts no
/// server resources.
///
/// # APIs Provided
///
/// _None._ It only `add_panel`s onto the host registry; it publishes no API.
///
/// # Dependencies
///
/// - [`TuiRegistry`] — the `polaris-tui` host API the panel registers into,
///   declared as a `requires` capability in [`access`](Plugin::access).
///
/// # Lifecycle
///
/// Feature-gated on `oxigraph` (the `tui` module is only compiled with that
/// feature). `build()` does two things: it installs a [`tracing_subscriber`]
/// global default (via [`try_init`](tracing_subscriber::util::SubscriberInitExt::try_init);
/// failure is logged to stderr and swallowed), then `add_panel`s a
/// focus-gated [`LogPanel`] onto the [`TuiRegistry`]. Because it claims the
/// global subscriber slot it conflicts with any other plugin that takes it —
/// including `polaris_tui`'s own [`LogPanelPlugin`] and
/// `polaris::plugins::TracingPlugin`. Pick exactly one.
///
/// # Extends
///
/// Does not stand alone — it contributes a panel to the `polaris-tui`
/// [`TuiPlugin`](polaris_tui::TuiPlugin) host surface via
/// [`TuiRegistry::add_panel`](polaris_tui::TuiRegistry::add_panel). Register
/// `TuiPlugin` first.
///
/// # Example
///
/// ```ignore
/// use polaris::system::server::Server;
/// use polaris_tui::{LayoutMode, TuiConfig, TuiPlugin};
/// use polaris_ontrace::FilteredLogPanelPlugin;
///
/// let mut server = Server::new();
/// server
///     .add_plugins(TuiPlugin::new(
///         TuiConfig::default().with_layout(LayoutMode::Tiled),
///     ))
///     // replaces polaris_tui's LogPanelPlugin — do not register both
///     .add_plugins(FilteredLogPanelPlugin::with_capacity(4_000));
/// ```
pub struct FilteredLogPanelPlugin {
    capacity: usize,
}

impl FilteredLogPanelPlugin {
    /// Construct with a custom ring-buffer capacity for the log panel.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { capacity }
    }
}

impl Default for FilteredLogPanelPlugin {
    fn default() -> Self {
        Self {
            capacity: DEFAULT_CAPACITY,
        }
    }
}

impl Plugin for FilteredLogPanelPlugin {
    const ID: &'static str = "ontrace::tui::filtered_log_panel";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        let (_buffer, layer, panel) = LogPanelPlugin::layer_only(self.capacity);
        // `polaris_ontrace` is this crate; the rest are Polaris crates whose
        // events describe agent / session / model / tool / graph steps.
        // Everything else is dropped.
        let filter = Targets::new()
            .with_target("polaris_ontrace", LevelFilter::TRACE)
            .with_target("polaris_ai", LevelFilter::INFO)
            .with_target("polaris_sessions", LevelFilter::INFO)
            .with_target("polaris_models", LevelFilter::INFO)
            .with_target("polaris_model_providers", LevelFilter::INFO)
            .with_target("polaris_tools", LevelFilter::INFO)
            .with_target("polaris_graph", LevelFilter::INFO)
            .with_target("polaris_agent", LevelFilter::INFO)
            .with_target("polaris_core_plugins", LevelFilter::INFO)
            .with_target("polaris_system", LevelFilter::WARN)
            .with_target("polaris_tui", LevelFilter::WARN)
            .with_default(LevelFilter::OFF);
        let subscriber = tracing_subscriber::registry().with(layer.with_filter(filter));
        if let Err(err) = subscriber.try_init() {
            eprintln!(
                "ontrace: failed to install FilteredLogPanelPlugin's tracing \
                 subscriber: {err}. Another global default is already set."
            );
        }
        server
            .api::<TuiRegistry>()
            .expect("FilteredLogPanelPlugin requires TuiPlugin")
            .add_panel(FocusGatedLogPanel::new(panel));
    }

    fn access(&self) -> PluginAccess {
        // `TuiRegistry` is an API mutated through its own `add_panel`, so this
        // is a `requires` (the panel ordering matches polaris-tui's own
        // panel plugins) rather than a `get_resource_mut` `extends`.
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}

/// Wraps a [`LogPanel`] so its `j`/`k`/Up/Down/Home/End shortcuts only fire
/// when the panel actually has focus.
///
/// The upstream [`LogPanel`] consumes those keys unconditionally; in tiled
/// layout that means typing into another panel also scrolls the logs. Gating
/// on the shell's FocusGained/FocusLost events restores the expected
/// "input only steers the focused panel" mental model.
struct FocusGatedLogPanel {
    inner: LogPanel,
    focused: bool,
}

impl FocusGatedLogPanel {
    fn new(inner: LogPanel) -> Self {
        Self {
            inner,
            focused: false,
        }
    }
}

impl TuiPanel for FocusGatedLogPanel {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn title(&self) -> &str {
        self.inner.title()
    }

    fn init(&mut self) {
        self.inner.init();
    }

    fn tick(&mut self) {
        self.inner.tick();
    }

    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome {
        match event {
            TuiEvent::FocusGained => {
                self.focused = true;
                self.inner.handle_event(event)
            }
            TuiEvent::FocusLost => {
                self.focused = false;
                self.inner.handle_event(event)
            }
            TuiEvent::Key(_) | TuiEvent::Mouse(_) if !self.focused => EventOutcome::Continue,
            _ => self.inner.handle_event(event),
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.inner.render(frame, area);
    }
}
