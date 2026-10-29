//! Built-in panels and their contribution plugins.
//!
//! These ship as a convenience and as a reference for how to write a
//! `TuiPanel`. Each panel lives behind its own contribution plugin so a
//! consumer can pick the subset they want; the framework crate
//! ([`polaris_tui`](crate)) does not register any of them by default.
//!
//! | Module           | Plugin                       | Use                                              |
//! |------------------|------------------------------|--------------------------------------------------|
//! | [`conversation`] | [`ConversationPanelPlugin`]  | Chat-style transcript driven by [`UserIO`](polaris::plugins::UserIO). |
//! | [`logs`]         | [`LogPanelPlugin`]           | Tail of `tracing` events fed by a subscriber layer. |
//! | [`sessions`]     | [`SessionsPanelPlugin`]      | Live list of active sessions from [`SessionsAPI`](polaris::sessions::SessionsAPI). |
//! | [`help`]         | [`HelpPanelPlugin`]          | Static keymap reference. Smallest possible example. |

pub mod conversation;
pub mod help;
pub mod logs;
pub mod sessions;

pub use conversation::{ConversationPanel, ConversationPanelPlugin, TuiIOProvider};
pub use help::{HelpPanel, HelpPanelPlugin};
pub use logs::{LogBuffer, LogEntry, LogLevel, LogPanel, LogPanelPlugin};
pub use sessions::{SessionsPanel, SessionsPanelPlugin};
