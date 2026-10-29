# Integration Guide

`polaris_ontrace` is a set of `polaris::Plugin`s you compose onto a
`polaris::Server` — not a standalone binary. This page indexes the common ways
to combine them and points at the three documentation standards that govern how
each piece is written.

The invariant across every pattern: register `TraceIngestPlugin` (the fan-in
dispatcher), add **one** extraction backend that claims its receiver, then add
as many source plugins as you like. Sources fan in over `TraceIngestAPI`; the
backend drains the channel serially. See `examples/stdin_demo.rs` for full
wiring.

## Common Integration Patterns

| Goal | Plugins to combine | APIs / Resources involved |
|------|--------------------|---------------------------|
| Pipe an upstream agent's stdout into the KG | `TraceIngestPlugin` + `ExtractionPlugin` + `StdinIOPlugin` | `TraceIngestAPI` (push), `KnowledgeGraph` (Global) |
| Ingest traces from a growing log file | `TraceIngestPlugin` + `ExtractionPlugin` + `FileTailIOPlugin` | `TraceIngestAPI`, `TraceLine` (per-trace Local), `KnowledgeGraph` |
| Receive OpenTelemetry spans/logs over HTTP | `TraceIngestPlugin` + a backend + `OtlpIOPlugin` | `TraceIngestAPI`; routes `POST /v1/traces`, `POST /v1/logs` |
| Follow a Claude Code session | `TraceIngestPlugin` + a backend + `ClaudeCodeIOPlugin` | `TraceIngestAPI` |
| Constrain extraction to a curated ontology | any backend `+ .with_seed(SeedOntology::load(dir)?)` | `SeedOntology` (consumer-supplied Local), `OntologyContext` |
| Persist triples as queryable RDF | `OxigraphPlugin` + `TraceIngestPlugin` + `OxigraphExtractionPlugin` | `OxigraphStore` (Global, feature `oxigraph`) |
| Query the KG in natural language | `OxigraphPlugin` + `ToolsPlugin` + `InspectPlugin` | `InspectAPI::ask`, `OxigraphStore` |
| Fan multiple transports into one KG | `TraceIngestPlugin` + one backend + several source plugins | `TraceIngestAPI` (open extension — many pushers) |

Backends are mutually exclusive: `ExtractionPlugin` and
`OxigraphExtractionPlugin` both call `TraceIngestRx::take`, and the second claim
panics. Pick one per server. The `oxigraph`-gated plugins
(`OxigraphExtractionPlugin`, `InspectPlugin`, the TUI panels) require the
`oxigraph` feature.

## Adding a Plugin, API, or Resource

When you contribute a new piece, document it against the matching standard so it
stays discoverable:

- **Adding a plugin** (a new source transport, backend, or inspector) — follow
  the [Plugin Documentation Standard](./plugins.md). Declare dependencies by
  capability in `access()`, fill the `# Resources Provided` / `# APIs Provided`
  / `# Dependencies` / `# Example` sections, and add the conditional
  `# Routes Provided` / `# Lifecycle` sections when they apply.
- **Adding an API** (a new `insert_api` handle) — follow the
  [API Documentation Standard](./api.md). Classify its `# Composition` as Open
  extension, Provider-scoped, or Single-replace, and show both the provider's
  `insert_api` and a consumer's `server.api::<T>()` in the `# Example`.
- **Adding a resource** (a new `GlobalResource` / `LocalResource`) — follow the
  [Resource Documentation Standard](./resources.md). State `# Scope`,
  `# Provided by`, the `# Access` / mutation contract, and `# Alternatives`, and
  write the doc from the consuming system's perspective.
