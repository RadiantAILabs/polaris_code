//! Per-trace input passed into an agent's session context.

use crate::resources::kg::Timestamp;
use polaris::system::resource::LocalResource;

/// A single raw trace line as ingested from a source — the per-session input
/// an extraction system reads.
///
/// Use this when a system needs the text (and observation metadata) of the
/// one line currently being extracted: the extractor reads it via
/// `Res<TraceLine>` to build its LLM prompt, and write paths read its
/// `observed_at` / `trace_id` to stamp the resulting triples.
///
/// # Scope
///
/// `Local` — each extraction runs in its own ephemeral session for exactly
/// one line, so the line is per-session input that must not leak between
/// sessions; durable state lives in the global resources instead.
///
/// # Provided by
///
/// Inserted per-trace by the extraction worker, not a plugin `build()`. The
/// worker (in [`crate::ExtractionPlugin`] / `OxigraphExtractionPlugin`) builds
/// it from each inbound message via
/// [`trace_line_from_message`](crate::plugins::trace_ingest) and inserts it
/// into the session context with
/// `sessions.run_oneshot(&agent, |ctx| ctx.insert(line))`.
///
/// # Access
///
/// Consume as `Res<TraceLine>`: **read-only per-session input**. It is rebuilt
/// fresh for every session, so there is no mutation contract — a system reads
/// the fields and never writes them.
///
/// # Alternatives
///
/// | Alternative | When |
/// | --- | --- |
/// | _none_ | This is the only per-trace input resource in the crate. |
///
/// # Example system
///
/// ```no_run
/// use polaris::system::system;
/// use polaris::system::param::Res;
/// use polaris_ontrace::TraceLine;
///
/// /// Read the current line's text to drive extraction.
/// #[system]
/// async fn inspect_line(line: Res<TraceLine>) {
///     tracing::debug!(text = %line.text, trace_id = ?line.trace_id, "extracting");
/// }
/// ```
#[derive(Debug, Clone)]
pub struct TraceLine {
    /// Raw text content of the line.
    pub text: String,
    /// When the line was observed (set by the ingest task).
    pub observed_at: Timestamp,
    /// Optional upstream trace identifier, parsed from the line if present.
    pub trace_id: Option<String>,
}

impl LocalResource for TraceLine {}
