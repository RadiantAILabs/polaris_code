//! Ratatui-backed interactive shell for [`InspectPlugin`].
//!
//! [`InspectTuiPlugin`] contributes two custom [`TuiPanel`]s to
//! [`polaris_tui::TuiRegistry`]:
//!
//! - [`chat::InspectChatPanel`] — wraps [`InspectAPI::ask`](crate::InspectAPI)
//!   in an input prompt + scrolling transcript.
//! - [`sparql::SparqlPanel`] — runs raw SPARQL against the
//!   [`OxigraphStore`](crate::oxigraph::OxigraphStore) (clone of the global)
//!   on a blocking task; results render as Turtle (`CONSTRUCT`/`DESCRIBE`) or
//!   SPARQL Results JSON (`SELECT`/`ASK`), matching the inspect agent's
//!   `sparql_query` tool.
//!
//! Pair it with [`polaris_tui::panels::LogPanelPlugin`] and
//! [`polaris_tui::panels::HelpPanelPlugin`] for the unified
//! conversation + SPARQL + logs + help shell. [`InspectPlugin`] is a
//! required dependency — without it, [`InspectAPI`] never appears and the
//! chat panel surfaces "inspect API not ready yet".
//!
//! [`InspectPlugin`]: crate::InspectPlugin

mod chat;
mod log;
mod sparql;

use crate::InspectAPI;
use crate::oxigraph::OxigraphStore;
use crate::plugins::inspect::InspectPlugin;
use parking_lot::Mutex;
use polaris::system::plugin::{Contract, Plugin, PluginAccess, PluginId, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::TuiRegistry;
use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

pub use chat::InspectChatPanel;
pub use log::FilteredLogPanelPlugin;
pub use sparql::SparqlPanel;

/// The polaris-tui shell for `ontrace inspect`: contributes the inspect-mode
/// panels — a chat transcript over [`InspectAPI::ask`] and a raw SPARQL panel
/// against the shared [`OxigraphStore`].
///
/// Add this when you want the interactive inspector shell. It registers two
/// [`TuiPanel`](polaris_tui::TuiPanel)s into the host `polaris-tui`
/// [`TuiRegistry`]:
///
/// - [`InspectChatPanel`](crate::tui::InspectChatPanel) — an input prompt plus
///   scrolling transcript that forwards each line to
///   [`InspectAPI::ask`](crate::plugins::inspect::InspectAPI::ask).
/// - [`SparqlPanel`](crate::tui::SparqlPanel) — runs raw SPARQL against a clone
///   of the global [`OxigraphStore`](crate::oxigraph::OxigraphStore) on a
///   blocking task, rendering Turtle (`CONSTRUCT`/`DESCRIBE`) or SPARQL Results
///   JSON (`SELECT`/`ASK`).
///
/// Pair it with [`LogPanelPlugin`](polaris_tui::panels::LogPanelPlugin) (or
/// this crate's [`FilteredLogPanelPlugin`](crate::tui::FilteredLogPanelPlugin))
/// and [`HelpPanelPlugin`](polaris_tui::panels::HelpPanelPlugin) for the
/// unified conversation + SPARQL + logs + help layout.
///
/// # Resources Provided
///
/// _None._ This plugin inserts no resources. It clones the existing
/// [`OxigraphStore`](crate::oxigraph::OxigraphStore) global into its
/// [`SparqlPanel`](crate::tui::SparqlPanel) but does not register anything new.
///
/// # APIs Provided
///
/// _None._ The chat panel *consumes* [`InspectAPI`](crate::plugins::inspect::InspectAPI)
/// (published by [`InspectPlugin`](crate::plugins::inspect::InspectPlugin));
/// this plugin publishes no API of its own.
///
/// # Dependencies
///
/// - [`TuiRegistry`] — the `polaris-tui` host API the panels register into
///   (declared as a `requires` capability in [`access`](Plugin::access)).
/// - [`OxigraphStore`](crate::oxigraph::OxigraphStore) — the global cloned in
///   `build()` to back the SPARQL panel, provided by
///   [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin) (a `requires`
///   capability).
/// - [`InspectPlugin`](crate::plugins::inspect::InspectPlugin) — provides the
///   [`InspectAPI`](crate::plugins::inspect::InspectAPI) the chat panel calls.
///   Declared in [`dependencies`](Plugin::dependencies) rather than `access()`
///   because the API is published in `InspectPlugin::ready()` and read in this
///   plugin's `ready()` — a ready→ready ordering with no build-phase provider
///   for capabilities to hang on. Without it the chat panel surfaces
///   "inspect API not ready yet".
///
/// # Lifecycle
///
/// Feature-gated on `oxigraph`. The plugin is two-phase:
///
/// - `build()` clones the [`OxigraphStore`](crate::oxigraph::OxigraphStore)
///   global, constructs both panels with a shared [`OnceLock<InspectAPI>`], and
///   `add_panel`s them onto the [`TuiRegistry`] *before* the host
///   `TuiPlugin::ready` drains it.
/// - `ready()` fills the [`OnceLock`] with the now-published
///   [`InspectAPI`](crate::plugins::inspect::InspectAPI). The chat panel shows a
///   placeholder until this happens, but in practice the gap is sub-millisecond.
///
/// # Extends
///
/// Does not stand alone — it contributes panels to the `polaris-tui`
/// [`TuiPlugin`](polaris_tui::TuiPlugin) host surface via
/// [`TuiRegistry::add_panel`](polaris_tui::TuiRegistry::add_panel). Register
/// `TuiPlugin` first; the panels appear in the shell's tiled layout.
///
/// # Example
///
/// ```ignore
/// use polaris::system::server::Server;
/// use polaris_tui::{LayoutMode, TuiConfig, TuiPlugin};
/// use polaris_tui::panels::HelpPanelPlugin;
/// use polaris_ontrace::{FilteredLogPanelPlugin, InspectPlugin, InspectTuiPlugin};
/// # use polaris_ontrace::oxigraph::OxigraphPlugin;
///
/// let mut server = Server::new();
/// server
///     // ... OxigraphPlugin, SessionsPlugin, ModelsPlugin, InspectPlugin ...
///     .add_plugins(InspectPlugin::new())
///     .add_plugins(TuiPlugin::new(
///         TuiConfig::default().with_layout(LayoutMode::Tiled),
///     ))
///     .add_plugins(FilteredLogPanelPlugin::default())
///     .add_plugins(HelpPanelPlugin)
///     .add_plugins(InspectTuiPlugin::new());
/// ```
pub struct InspectTuiPlugin {
    inspect_api: Arc<OnceLock<InspectAPI>>,
}

impl InspectTuiPlugin {
    /// Construct the plugin. The shared [`OnceLock`] is populated in
    /// [`ready`](Plugin::ready) and then read by the chat panel on every
    /// `Enter`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inspect_api: Arc::new(OnceLock::new()),
        }
    }
}

impl Default for InspectTuiPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for InspectTuiPlugin {
    const ID: &'static str = "ontrace::tui::inspect";
    const VERSION: Version = Version::new(0, 1, 0);

    fn access(&self) -> PluginAccess {
        // Both consumed in `build()`: the `OxigraphStore` clone backing the
        // SPARQL panel and the `TuiRegistry` the panels register into. The
        // `InspectAPI` read in `ready()` is produced by `InspectPlugin::ready()`
        // — a ready→ready ordering capabilities can't express (it is not a
        // build-phase `provides`), so it stays in `dependencies()` below.
        PluginAccess::new()
            .requires::<OxigraphStore>(VersionReq::caret(OxigraphStore::CONTRACT_VERSION))
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }

    fn build(&self, server: &mut Server) {
        let store = server
            .get_global::<OxigraphStore>()
            .expect("InspectTuiPlugin requires OxigraphPlugin (OxigraphStore global)")
            .clone();
        let transcript: Arc<Mutex<VecDeque<chat::ChatLine>>> =
            Arc::new(Mutex::new(VecDeque::new()));
        let chat = InspectChatPanel::new(self.inspect_api.clone(), transcript);
        let sparql = SparqlPanel::new(store);
        server
            .api::<TuiRegistry>()
            .expect("InspectTuiPlugin requires TuiPlugin")
            .add_panel(chat)
            .add_panel(sparql);
    }

    async fn ready(&self, server: &mut Server) {
        let api = server
            .api::<InspectAPI>()
            .expect("InspectTuiPlugin requires InspectPlugin (InspectAPI)")
            .clone();
        let _ = self.inspect_api.set(api);
    }

    fn dependencies(&self) -> Vec<PluginId> {
        // `InspectAPI` is produced in `InspectPlugin::ready()` and read in this
        // plugin's `ready()`; the `OxigraphStore`/`TuiRegistry` capabilities in
        // `access()` cover the build-phase providers, but this ready→ready edge
        // has no build-phase capability to hang on, so it remains here.
        vec![PluginId::of::<InspectPlugin>()]
    }
}
