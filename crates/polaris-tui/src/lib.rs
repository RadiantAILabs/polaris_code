//! Composable terminal UI for Polaris.
//!
//! Plugins contribute in-process [`TuiPanel`]s (ratatui widgets plus an event
//! handler) to a shared [`TuiRegistry`]. [`TuiPlugin`] drains the registry
//! in `ready()` and spawns a background tokio task that owns the terminal,
//! pumps crossterm events, and renders the panels in either a single-view
//! or tiled layout — toggleable at runtime.
//!
//! `TuiRegistry` holds *in-process trait objects*: plugins add panels to it
//! during their build phase.
//!
//! # Quick Start
//!
//! ```no_run
//! use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
//! use polaris::system::server::Server;
//! use polaris_tui::{
//!     EventOutcome, TuiConfig, TuiEvent, TuiPanel, TuiPlugin, TuiRegistry,
//! };
//! use ratatui::Frame;
//! use ratatui::layout::Rect;
//! use ratatui::widgets::Paragraph;
//!
//! struct HelloPanel;
//!
//! impl TuiPanel for HelloPanel {
//!     fn id(&self) -> &str { "hello" }
//!     fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
//!         frame.render_widget(Paragraph::new("hello, polaris"), area);
//!     }
//! }
//!
//! struct HelloContribution;
//!
//! impl Plugin for HelloContribution {
//!     const ID: &'static str = "myapp::hello_tui";
//!     const VERSION: Version = Version::new(0, 1, 0);
//!
//!     fn build(&self, server: &mut Server) {
//!         server
//!             .api::<TuiRegistry>()
//!             .expect("TuiPlugin provides TuiRegistry; the resolver orders it first")
//!             .add_panel(HelloPanel);
//!     }
//!
//!     fn access(&self) -> PluginAccess {
//!         PluginAccess::new()
//!             .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
//!     }
//! }
//!
//! # async fn run() {
//! let mut server = Server::new();
//! server
//!     .add_plugins(TuiPlugin::new(TuiConfig::default()))
//!     .add_plugins(HelloContribution);
//! server.run().await;
//! # }
//! ```
//!
//! # Global Keybindings
//!
//! The shell reserves a small chord table so plugin panels do not need to
//! coordinate on them:
//!
//! | Chord            | Action                                  |
//! |------------------|-----------------------------------------|
//! | `Ctrl-C` / `Ctrl-Q` | Quit the TUI.                        |
//! | `Tab` / `Shift-Tab` | Cycle panel focus (a panel with an open completion popup claims them first). |
//! | `Alt-1` … `Alt-9` | Jump directly to panel N.              |
//! | `Ctrl-L`         | Toggle Single ↔ Tiled layout.          |

#![warn(missing_docs)]

mod app;
mod composer;
mod config;
mod event;
mod layout;
mod panel;
mod plugin;
mod registry;
mod runtime;

pub mod nav;
pub mod widgets;

#[cfg(feature = "panels")]
pub mod commands;
#[cfg(feature = "panels")]
pub mod panels;

/// Discoverability catalogs: indexes of every plugin, API, and consumer-facing
/// resource this crate ships, each linking to the item's rustdoc.
///
/// These are doc-only modules whose contents are authored in
/// `src/docs/{plugins,apis,resources}.md` and kept in sync with the source by
/// the drift guards in `tests/{plugin,api,resource}_catalog.rs`. See the
/// reference standards under `docs/reference/` for what each item's rustdoc
/// must contain.
pub mod docs {
    #[doc = include_str!("docs/plugins.md")]
    pub mod plugins {}

    #[doc = include_str!("docs/apis.md")]
    pub mod apis {}

    #[doc = include_str!("docs/resources.md")]
    pub mod resources {}
}

pub use composer::editor::EditorCommand;
#[cfg(feature = "panels")]
pub use composer::history::{PromptHistoryConfig, PromptHistoryNamespace, PromptHistoryStore};

pub use config::{LayoutMode, TuiConfig};
pub use event::TuiEvent;
pub use nav::{KeyMap, NavIntent};
pub use panel::{EventOutcome, TuiPanel};
pub use plugin::{TuiHandle, TuiPlugin};
pub use registry::TuiRegistry;
pub use widgets::ScrollableTranscript;

// Re-export the ratatui surface panels need so consumers can write panels
// without pinning their own ratatui version (the framework's version is
// what the runtime actually drives).
pub use ratatui;

// `TuiEvent::Key`/`Mouse` carry crossterm types, so re-export crossterm too —
// consumers matching on those events shouldn't have to add (and version-match)
// their own crossterm dependency.
pub use ratatui::crossterm;

#[cfg(feature = "panels")]
mod transcript;
