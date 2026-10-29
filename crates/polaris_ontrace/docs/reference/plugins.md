# Plugin Documentation Standard

`polaris_ontrace` ships as a bag of `polaris::Plugin`s — transport sources, a
fan-in dispatcher, and extraction/inspection backends — that a consumer wires
into a `polaris::Server`. Every plugin's rustdoc is a contract: it tells a
downstream consumer what the plugin puts on the server, what it expects to
already be there, and how to register it. This page is the normative standard
for plugin docs in this crate.

## Documentation Standard

A plugin's module- or struct-level rustdoc MUST open with a one- or two-sentence
**summary that states what the plugin does AND when to reach for it** (e.g.
"runs an OTLP/HTTP receiver and forwards each span to the extractor — use it
when an upstream agent emits OpenTelemetry"). Past the summary, the following
sections apply.

### Required sections

Flag an ISSUE if any of these is missing.

| Section | Content |
|---------|---------|
| Summary + when-to-use | Opening paragraph: what it does **and** when a consumer would pick it. |
| `# Resources Provided` | Table `Resource \| Scope \| Description`. If the plugin registers no globals/locals, a single `_none_` row with a one-line reason. |
| `# APIs Provided` | Table `API \| Description`. `_none_` row if it publishes no `insert_api`. |
| `# Dependencies` | `None.`, or a list of the plugins/capabilities it needs. Prefer naming the **capability** (`requires::<T>()` in `access()`) over the providing plugin, since the resolver binds by type. |
| `# Example` | A `no_run` rustdoc block showing `add_plugins(...)` registration in dependency order plus the typical follow-up call. |

### Conditional sections

Flag an ISSUE only when the trigger applies and the section is missing.

| Section | Trigger |
|---------|---------|
| `# Routes Provided` | The plugin binds HTTP routes. Table `Method \| Path \| Description`. |
| `# Tools Provided` | It registers `#[tool]` / `toolset` with a `ToolRegistry`. |
| `# Hooks Registered` | It registers session/server hooks. |
| `# Middleware Registered` | It installs middleware. |
| `# Lifecycle` | It has a non-trivial `ready()`/`cleanup()`, spawns a background task, claims a one-shot resource, or is feature-gated. Bullet list of what happens and when. |
| `# Extends` | It contributes to another plugin's API or registry rather than owning its own. |

## Canonical Exemplars

Read these in `src/` before reviewing a new plugin — they are the shape to match.

- **`StdinIOPlugin`** (`crate::StdinIOPlugin`, `src/sources/stdin.rs`) — minimal
  source. Provides no resources or APIs; depends purely by capability on
  `TraceIngestAPI` (`requires::<TraceIngestAPI>()`). Its `# Resources Provided`
  and `# APIs Provided` would both be `_none_`. The model for "a plugin that
  only consumes a capability and spawns a forwarder."
- **`FileTailIOPlugin`** (`crate::FileTailIOPlugin`, `src/sources/file_tail.rs`)
  — source with a real `# Lifecycle`: `ready()` spawns a `tail -F` follow task
  that reopens the file each poll to survive truncation/rotation. The exemplar
  for documenting a spawned background task and its shutdown condition.
- **`OtlpIOPlugin`** (`crate::OtlpIOPlugin`, `src/sources/otlp.rs`) — source
  that binds an `axum` HTTP receiver on `127.0.0.1:4318`. The exemplar for
  `# Routes Provided` (`POST /v1/traces`, `POST /v1/logs`) **and** `# Lifecycle`
  (bind-failure is logged and the task exits — not a panic).
- **`ExtractionPlugin`** (`crate::ExtractionPlugin`, `src/plugins/extraction.rs`)
  — full in-memory backend. Registers four globals (`KnowledgeGraph`,
  `Aliases`, `OntologyGap`, `Spend`), claims the ingest receiver
  (`requires::<TraceIngestRx>()`), and runs the serial worker. The exemplar for
  a complete `# Resources Provided` table plus a `# Lifecycle` describing the
  one-shot `take()` and the per-trace `run_oneshot`.
- **`OxigraphExtractionPlugin`** (`crate::OxigraphExtractionPlugin`,
  `src/plugins/oxigraph_extraction.rs`) — feature-gated (`oxigraph`) backend
  variant. The exemplar for documenting a feature gate in `# Lifecycle` and for
  noting that `OxigraphStore` comes from a required `OxigraphPlugin` rather than
  being provided here.
- **`TraceIngestPlugin`** (`crate::TraceIngestPlugin`,
  `src/plugins/trace_ingest.rs`) — API provider. Publishes `TraceIngestAPI`
  (sources push) and `TraceIngestRx` (one backend takes) in `build()` and
  declares both via `provides::<...>()` in `access()`. The exemplar for a plugin
  whose whole job is to put two APIs on the server.

## Anti-Patterns

- **Insertion-order coupling.** Documenting "register this after the backend"
  instead of declaring the dependency in `access()`. The resolver binds
  `requires`/`provides` by type and errors at `finish()`; prose ordering is not
  a contract. Sources here say they need `TraceIngestAPI`, not "register me
  after `ExtractionPlugin`."
- **Circular dependencies.** A plugin requiring a capability it also provides,
  or two plugins each requiring the other. The `access()` resolver cannot order
  these.
- **Happy-path-only docs.** Omitting the failure and no-op modes. A correct
  plugin doc surfaces: what `build()`/`ready()` does when a required provider is
  absent (here, a typed `ServerBuildError` at `finish()`, not a runtime panic);
  what a feature-gated plugin compiles to without its feature; and the
  consequence of a one-shot resource being claimed twice (`TraceIngestRx::take`
  panics on the second backend — that is documented intentionally).
