//! Agent-side Polaris plugins.
//!
//! Each plugin here wires one agent pattern from [`crate::agents`] into a
//! `polaris::system::server::Server`: registers the agent on `SessionsAPI`,
//! installs the global resources it depends on (from [`crate::resources`]),
//! and drives the per-trace worker loop.
//!
//! Transport (source) plugins live in [`crate::sources`] — they don't run
//! agents themselves, they push into the `TraceIngestAPI` published by
//! [`trace_ingest::TraceIngestPlugin`]. Whichever backend plugin
//! ([`extraction::ExtractionPlugin`] or
//! [`oxigraph_extraction::OxigraphExtractionPlugin`]) is also registered
//! claims the receiver and drains it.

pub mod extraction;
pub mod trace_ingest;

#[cfg(feature = "oxigraph")]
pub mod inspect;
#[cfg(feature = "oxigraph")]
pub mod oxigraph_extraction;
