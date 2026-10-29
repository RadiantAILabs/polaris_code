//! Trace transport plugins.
//!
//! Each submodule defines an [`IOProvider`](polaris::plugins::IOProvider)
//! and a Polaris [`Plugin`](polaris::system::plugin::Plugin) that drives it
//! in a background task, pushing inbound messages into the
//! [`TraceIngestAPI`](crate::plugins::trace_ingest::TraceIngestAPI) registered by
//! [`TraceIngestPlugin`](crate::plugins::trace_ingest::TraceIngestPlugin).
//!
//! Multiple source plugins can be active on the same server — they share
//! the single fan-in channel. Pick whichever combination makes sense for
//! the deployment: stdin pipe, log-file tail, OTLP HTTP receiver, or the
//! higher-level [`ClaudeCodeIOPlugin`](claude_code::ClaudeCodeIOPlugin)
//! convenience wrapper.

pub mod claude_code;
pub mod file_tail;
pub mod otlp;
pub mod stdin;

pub use claude_code::ClaudeCodeIOPlugin;
pub use file_tail::FileTailIOPlugin;
pub use otlp::OtlpIOPlugin;
pub use stdin::StdinIOPlugin;
