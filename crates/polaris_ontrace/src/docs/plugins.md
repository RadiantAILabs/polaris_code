A catalog of every [`Plugin`](polaris::system::plugin::Plugin) shipped by
`polaris-ontrace`, grouped by the side of the pipeline it sits on.

Each entry links to the plugin's rustdoc page, which is the canonical source
for its provided resources, APIs, dependencies, lifecycle, and a registration
example per the [plugin documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_ontrace/docs/reference/plugins.md#documentation-standard).
See the [integration guide](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_ontrace/docs/reference/guide.md)
to go the other way — from a downstream goal to the plugins that satisfy it.

# Sources — transports

Each source implements [`IOProvider`](polaris::plugins::IOProvider) and, in
`ready()`, spawns a task that drives the provider and pushes records into
[`TraceIngestAPI`](crate::TraceIngestAPI). They depend by capability on that
API, so they are indifferent to which extraction backend is wired up — compose
as many as you like into one server.

| Plugin | What it enables |
|--------|-----------------|
| [`StdinIOPlugin`](crate::StdinIOPlugin) | Reads `tokio::io::stdin()` line-by-line, pushing each line as a trace. Minimal source — no resources, no APIs. |
| [`FileTailIOPlugin`](crate::FileTailIOPlugin) | `tail -F` semantics over one or more files: follows appends and reopens across truncation/rotation. |
| [`OtlpIOPlugin`](crate::OtlpIOPlugin) | An embedded axum OTLP/HTTP JSON receiver on `127.0.0.1:4318`; decomposes `ResourceSpans`/`ResourceLogs` into one trace per span/log record. |
| [`ClaudeCodeIOPlugin`](crate::ClaudeCodeIOPlugin) | Convenience wrapper over file-tail / stdin with `~/.claude/projects/` session discovery for tailing Claude Code transcripts. |

# Ingest — the dispatcher

| Plugin | What it enables |
|--------|-----------------|
| [`TraceIngestPlugin`](crate::TraceIngestPlugin) | Owns the mpsc channel that fans sources into one backend. Provides [`TraceIngestAPI`](crate::TraceIngestAPI) (sources push) and [`TraceIngestRx`](crate::plugins::trace_ingest::TraceIngestRx) (one backend claims). Knows nothing about agents or graph backends. |

# Extraction backends

Exactly one backend may be registered — it claims [`TraceIngestRx`](crate::plugins::trace_ingest::TraceIngestRx)
and drains the channel, running one extraction agent per trace.

| Plugin | What it enables |
|--------|-----------------|
| [`ExtractionPlugin`](crate::ExtractionPlugin) | The naive in-memory backend: registers the global [`KnowledgeGraph`](crate::KnowledgeGraph), [`Aliases`](crate::Aliases), [`Sightings`](crate::Sightings), [`OntologyGap`](crate::OntologyGap), and [`Spend`](crate::Spend), then runs a serial worker that extracts triples per line. |
| [`OxigraphExtractionPlugin`](crate::OxigraphExtractionPlugin) *(feature `oxigraph`)* | The RDF-backed backend variant: writes `ABox` quads into [`OxigraphStore`](crate::oxigraph::OxigraphStore), snapshots the `TBox` for ontology-aware extraction, and runs a SHACL-driven repair loop. |

# Oxigraph store & SPARQL tools

| Plugin | What it enables |
|--------|-----------------|
| [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin) *(feature `oxigraph`)* | Registers the global [`OxigraphStore`](crate::oxigraph::OxigraphStore), pre-loads ontology Turtle into the `TBox`, and (with `with_tools`) contributes a SPARQL toolset (`sparql_query`, `sparql_update`, `load_ontology_turtle`, `describe_resource`) to the [`ToolRegistry`](polaris::tools::ToolRegistry). |

# Inspect — natural-language Q&A

| Plugin | What it enables |
|--------|-----------------|
| [`InspectPlugin`](crate::InspectPlugin) *(feature `oxigraph`)* | Registers an agent that answers questions about the graph over SPARQL, exposed as [`InspectAPI`](crate::InspectAPI). Mutually exclusive with the extraction backends — it reads an existing store rather than ingesting. |

# TUI — the `ontrace inspect` shell

These are panel-contributing extenders: they add panels to a
`polaris_tui::TuiPlugin` host rather than
standing alone.

| Plugin | What it enables |
|--------|-----------------|
| [`InspectTuiPlugin`](crate::InspectTuiPlugin) *(feature `oxigraph`)* | Contributes the chat panel (over [`InspectAPI`](crate::InspectAPI)) and a raw-SPARQL panel (over a clone of [`OxigraphStore`](crate::oxigraph::OxigraphStore)) to the inspect shell. |
| [`FilteredLogPanelPlugin`](crate::FilteredLogPanelPlugin) *(feature `oxigraph`)* | A target-filtered log panel for the inspect shell; a focus-gated drop-in for the stock log panel that shows only `polaris-ontrace`/`polaris-ai` events. |
