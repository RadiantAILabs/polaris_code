# AGENTS.md (CLAUDE.md)

This file provides guidance to coding agents working with code in this crate.

## Commands

Run from the workspace root:

```sh
cargo check -p polaris_ontrace --all-targets
cargo test -p polaris_ontrace                                  # default feature set — full suite, includes ontology/schema tests
cargo test -p polaris_ontrace --no-default-features            # naive in-memory backend only (no oxigraph)
cargo test -p polaris_ontrace --test <name> -- <filter>        # run a single test by substring
cargo clippy -p polaris_ontrace --all-targets -- -D warnings   # CI-equivalent lint gate
cargo clippy -p polaris_ontrace --all-targets --no-default-features -- -D warnings
cargo run -p polaris_ontrace --example stdin_demo              # end-to-end demo (requires ANTHROPIC_API_KEY)
```

`oxigraph` is in the default feature set, so `cargo test` / `cargo clippy` cover the full surface. Drop it with `--no-default-features` to gate the naive in-memory `KnowledgeGraph` path on its own. `examples/stdin_demo.rs` only uses the naive `ExtractionPlugin`, so it builds either way.

Clippy is treated as a wall: `-D warnings` plus `missing_docs = "warn"` and the `doc_markdown` / `semicolon_if_nothing_returned` lints in `Cargo.toml` mean every public item needs a doc comment and code shape matters. `cargo check` is not sufficient before committing.

The `stdin_demo` reads NDJSON or prose from stdin, makes one Haiku call per line (~1–2s each), prints KG size every 5s, and on Ctrl-C dumps RDF Turtle to `./kg.ttl`. It blocks until Ctrl-C even after stdin EOF — that is intentional, not a hang.

### Environment

`ANTHROPIC_API_KEY` must be set in a `.env` file in this directory (or any ancestor) — `dotenvy::dotenv()` loads it at demo startup. Without it, `AnthropicPlugin::from_env` panics during `Server::finish`.

`RUST_LOG=polaris_ontrace=trace,polaris=info` for verbose tracing.

## Architecture

The crate is a collection of **Polaris plugins**, not a standalone binary. They plug into a `polaris::Server` that already has `PersistencePlugin`, `ModelsPlugin` + `AnthropicPlugin`, and `SessionsPlugin` registered (see `examples/stdin_demo.rs` for the full wiring). Reading any single source file in isolation will be misleading — the data flow crosses task boundaries via tokio mpsc, an `API` handle, and Polaris's `SystemContext`.

There are three cleanly separable sides:

- **Ingest dispatcher** (`src/plugins/trace_ingest.rs`): owns the mpsc channel. Publishes `TraceIngestAPI` (sources push into it) and `TraceIngestRx` (one backend `take()`s the receiver). Knows nothing about agents or backends.
- **Backend side** (`src/agents/`, `src/plugins/extraction.rs`, `src/plugins/oxigraph_extraction.rs`): owns the KG and the per-trace extraction agent. Claims the receiver and drains it. Exactly one backend may be registered — a second `take()` panics.
- **Transport side** (`src/sources/`): one plugin per transport. Each implements `polaris::plugins::IOProvider` and, in `ready()`, spawns a task that drives the provider and pushes into `TraceIngestAPI`. Source plugins depend only on `TraceIngestPlugin`, so they're indifferent to which backend is wired up.

End-to-end flow:

```text
StdinIOPlugin    ─┐                                                ┌── ExtractionPlugin
FileTailIOPlugin ─┼─► TraceIngestAPI ─► mpsc<IOMessage> ─► take ──┤   or
OtlpIOPlugin     ─┤    (TraceIngestPlugin)                         └── OxigraphExtractionPlugin
ClaudeCodeIOPlug ─┘                                                       │
                                                                          ▼
                                          sessions.run_oneshot(<Agent>, |ctx| {
                                              ctx.insert(TraceLine);
                                              ctx.insert(AgentConfig);
                                              ctx.insert(SeedOntology);     // if `.with_seed(...)`
                                              ctx.insert(OntologyConfig);   // oxigraph plugin only
                                          })
                                                                          │
                                                                          ▼
                                          extract_triples ──► write_to_kg ──► KnowledgeGraph
                                             (Haiku call)     (Out<Vec>)       or OxigraphStore
                                          reads Res<OntologyContext>,
                                          built once in agent setup via
                                          OntologyContext::from_curated(...)
```

Five things only become clear after reading multiple files:

1. **Per-trace ephemeral sessions.** `ExtractionPlugin::ready` (`src/plugins/extraction.rs`) spawns a single worker that calls `sessions.run_oneshot::<usize>(...)` for every line. The agent is **stateless across traces** — durable state lives only in `KnowledgeGraph`. So adding cross-trace state (e.g. entity canonicalization tables) means another `GlobalResource`, not agent fields.

2. **Two-system graph.** `ExtractionAgent::build` registers two systems: `extract_triples` produces `Vec<RawTriple>`, `write_to_kg` consumes it via `Out<Vec<RawTriple>>` and writes the global KG. Polaris's `Graph` wires them by the in/out type signatures — there is no explicit `.then()` call. `extract_triples` also reads `Res<OntologyContext>` so the prompt and JSON schema are shaped by curated content when present (see #5).

3. **Resource scopes matter.** `TraceLine`, `AgentConfig`, `AgentLlm`, `OntologyContext`, and `SeedOntology` are `LocalResource` (per-session); `KnowledgeGraph`, `Aliases`, and `OxigraphStore` are `GlobalResource` (shared, accessed via `Res<T>` with `parking_lot::RwLock` for interior mutability — `ResMut<T>` is not available for globals). `AgentLlm` and `OntologyContext` are created in each agent's `setup` from the config locals + globals and inserted as `LocalResource`s. The naive in-memory agent reads any inserted `SeedOntology` (set via `ExtractionPlugin::with_seed`); the oxigraph agent snapshots it from the store's TBox graph.

4. **Fan-in over `TraceIngestAPI`, not `UserIO`.** Polaris's `UserIO` is a `LocalResource` — one factory per server — so it can't host multiple sources at once. Instead, `TraceIngestPlugin` exposes an `API` handle holding an `mpsc::Sender<IOMessage>`, modeled on the `TerminalIO::push` pattern in `polaris-ai`'s example terminal-IO plugin. Source plugins fetch the API in their `ready()` and forward into it. Each source still implements `IOProvider` for the typed message contract. Dependencies are declared by **capability** (`Plugin::access()`): `TraceIngestPlugin` `provides` `TraceIngestAPI` (sources `requires` it) and `TraceIngestRx` (the backend `requires` it), so a source depends on the resource type it actually consumes rather than naming a backend — the source side stays indifferent to which backend is wired up, and the resolver validates the provider exists at `finish()` (a typed `ServerBuildError`) instead of panicking at runtime.

5. **Ontology-aware extraction is per-session-snapshot, not per-line, and shared across backends.** Both agents land their curated predicates and classes in the same `OntologyContext` (built via `OntologyContext::from_curated`), so `extract_triples` is identical across backends. Where the snapshot comes from differs: the naive agent reads `SeedOntology` (inserted by the plugin worker if `.with_seed(...)` was called); the oxigraph agent runs a SPARQL `SELECT` against the TBox graph in its `setup`. `extract_triples` folds the summary into the system prompt and builds a JSON schema with a closed `enum` over the predicates — Strict drops irrelevant lines (LLM returns empty `triples`), CatchAll appends a sentinel predicate to the enum. With no seed and no TBox, the context is default-empty and the path collapses to free-form extraction (no ontology summary, unconstrained predicate). Seed packs live in `ontologies/`; the loader (`src/agents/seed.rs`) takes a directory path — paths are never baked into the library.

The single worker drains the channel serially — every Haiku call blocks the next line. Parallelism would be a `JoinSet` fan-out in the worker loop, not an Anthropic-side knob.

## Dependency wiring

`polaris` in the source is the [Polaris](https://github.com/RadiantAILabs/polaris) (`polaris-ai`) crate, inherited from the single `[workspace.dependencies] polaris` dependency in the root `Cargo.toml` (`polaris = { workspace = true, … }`) and renamed via `package = "polaris-ai"` there so call sites stay short. Do not add a separate `polaris-ai` requirement to this crate: every crate in the workspace must share one package instance.

When investigating Polaris APIs (`SessionsAPI`, `Agent`, `#[system]`, `GlobalResource`), read the Polaris docs for the version in the root `Cargo.toml` (docs.rs) or run `cargo doc --open -p polaris-ai`.

### Polaris semantic mappings

Quick lookups for the Polaris concepts referenced throughout this crate.

| Concept (as used here) | Polaris module |
| --- | --- |
| Prelude (re-exports of common types) | `polaris::prelude` |
| `Server`, `Plugin`, `#[system]`, `SystemContext` | `polaris::system` |
| `Res<T>`, `ResMut<T>`, `Out<T>` (used by `extract_triples` / `write_to_kg`) | `polaris::system` |
| `Graph` (used by `ExtractionAgent::build`) | `polaris::graph` |
| `Agent` trait — what `ExtractionAgent` implements | `polaris::agent` |
| `SessionsAPI`, `SessionsPlugin`, `run_oneshot` | `polaris::sessions` |
| `IOProvider`, `IOMessage`, `UserIO` (the trait surface each source plugin implements) | `polaris::plugins` |
| `PersistencePlugin`, `TimePlugin`, `TracingPlugin` | `polaris::plugins` |
| `ModelsPlugin`, `ModelRegistry`, `AnthropicPlugin` | `polaris::models` |
| `GlobalResource` / `LocalResource` scopes (used by `KnowledgeGraph`, `Aliases`, `TraceLine`) | `polaris::system::resource` |

### Polaris import paths

Paths as used in this crate (the `polaris` rename is in effect):

```rust
use polaris::prelude::Out;                          // Out<T> — inter-system data flow
use polaris::system::system;                        // #[system] macro
use polaris::system::param::{Res, SystemContext};
use polaris::system::resource::{GlobalResource, LocalResource};
use polaris::system::server::Server;
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::api::API;
use polaris::graph::Graph;
use polaris::agent::{Agent, SetupError};
use polaris::sessions::{SessionsAPI, SessionsPlugin, AgentTypeId};
use polaris::sessions::store::memory::InMemoryStore;
use polaris::models::{ModelsPlugin, AnthropicPlugin, ModelRegistry};
use polaris::models::llm::{Llm, LlmRequest, Message};
use polaris::plugins::{IOProvider, IOMessage, IOContent, IOError, IOSource, PersistencePlugin};
use polaris::tools::{ToolsPlugin, ToolRegistry, ToolPermission, toolset};
```

Differences from what the `polaris-ai` docs lead you to expect: `Out` lives in `polaris::prelude`, not `system::param`; this crate calls LLMs with an explicit `LlmRequest` struct + `llm.generate(req).await`, not the `builder()…generate()` chain; `ResMut` is not used at all (see below).

### Polaris resource params

| Param | Access | Registered via | Used here for |
| --- | --- | --- | --- |
| `Res<T>` | read-only | `server.insert_global(T)` (global) or `ctx.insert(T)` in agent `setup` (local) | globals: `KnowledgeGraph`, `Aliases`, `Sightings`, `Spend`, `OxigraphStore`; locals: `TraceLine`, `AgentConfig`, `AgentLlm`, `OntologyContext`, `SeedOntology` |
| `ResMut<T>` | mutable | `server.register_local(\|\| T)` | not used — globals mutate through `parking_lot::RwLock` interiors, locals are rebuilt fresh per session |
| `Out<T>` | read-only | automatic from a system's return type | `Out<Vec<RawTriple>>` carries `extract_triples` → `write_to_kg` |

The framework wires systems by in/out type signatures — there is no explicit `.then()`. A god struct with `Option` fields for each step's result defeats this; use `Res<T>` for input, `Out<T>` for inter-system flow, and a `GlobalResource` for genuinely durable state.

## Module map

| File | Role |
| ------ | ------ |
| `src/resources/kg.rs` | `KnowledgeGraph` (GlobalResource), `Triple`, `Entity`, `Predicate` |
| `src/resources/aliases.rs` | `Aliases` (GlobalResource), `Resolution` — entity canonicalization + `owl:sameAs` emission |
| `src/resources/trace_line.rs` | `TraceLine` (LocalResource) |
| `src/agents/extraction.rs` | `ExtractionAgent`, `AgentConfig`, `AgentLlm`, `OntologyContext`, `OntologyPolicy`, `RawTriple`, `extract_triples`, `write_to_kg`, LLM prompt + JSON schema |
| `src/agents/seed.rs` | `SeedOntology` (LocalResource) + `load` / `load_dir` / `from_string` / `merged` — runtime loader over `.ttl` files; default policy Strict |
| `src/agents/oxigraph_extraction.rs` | `OxigraphExtractionAgent`, `OntologyConfig`, `snapshot_ontology`, `write_to_oxigraph` (feature: `oxigraph`) |
| `src/plugins/trace_ingest.rs` | `TraceIngestPlugin`, `TraceIngestAPI` (sources push), `TraceIngestRx` (backend takes), `trace_line_from_message` — the mpsc dispatcher |
| `src/plugins/extraction.rs` | `ExtractionPlugin`, `with_model` / `with_seed` — in-memory backend; claims `TraceIngestRx` and drives the worker |
| `src/plugins/oxigraph_extraction.rs` | `OxigraphExtractionPlugin`, `with_policy` / `with_seed` (loads Turtle into TBox at `ready()`) — oxigraph backend (feature: `oxigraph`) |
| `src/oxigraph/` | `OxigraphStore`, `OxigraphPlugin` (`with_ontology_file`), `OxigraphSession`, `OxigraphTools` SPARQL toolset (feature: `oxigraph`) |
| `src/sources/mod.rs` | Source-plugin module shell + re-exports |
| `src/sources/stdin.rs` | `StdinIOPlugin` + `StdinProvider` (`IOProvider`) — reads `tokio::io::stdin()` |
| `src/sources/file_tail.rs` | `FileTailIOPlugin` + `FileTailProvider` — `tail -F` semantics with reopen/truncation |
| `src/sources/otlp.rs` | `OtlpIOPlugin` — `axum`-backed OTLP/HTTP JSON receiver on `127.0.0.1:4318`; decomposes `ResourceSpans`/`ResourceLogs` into per-record `IOMessage`s |
| `src/sources/claude_code.rs` | `ClaudeCodeIOPlugin` — convenience constructors over file tail / stdin, plus `~/.claude/projects/` discovery (`list_sessions`, `find_session_by_id`, `newest_session_anywhere`) |
| `src/iri.rs` | Shared IRI minting (`urn:ontrace:e/…`, `urn:ontrace:p/…`) used by dump + oxigraph agent |
| `src/dump.rs` | `write_turtle` — RDF Turtle serializer with percent-encoded URNs |
| `src/tui/` | `InspectTuiPlugin`, `InspectChatPanel`, `SparqlPanel` — polaris-tui panels for the `ontrace inspect` shell (chat over `InspectAPI::ask`, raw SPARQL against the shared `OxigraphStore`); feature: `oxigraph` |
| `src/bin/ontrace.rs` | CLI front-end — clap-derived `tail` / `inspect` / `query` / `list` subcommands; bare `ontrace` / `ontrace <session-id>` are `tail` shortcuts via clap's `external_subcommand`. `ontrace inspect <ttl>` launches the polaris-tui shell (chat + SPARQL + logs + help); `--ask <q>` short-circuits to a non-TUI one-shot answer |
| `ontologies/*.ttl` | Layered starter packs (`core`, `llm`, `agent`, `harness`, `user`, `coding`, `eval`, `tracing`) — loaded at runtime via `SeedOntology::load` / `load_dir`; consumers may keep their own directory elsewhere. `core` carries `Resolution` + `resolved_by` / `superseded_by` for closing open items (Intent, Question, ToolCall, EvalRun), and `child_of` as the generic parent/child DAG edge shared by tracing Spans and conversation Messages. `harness` holds vendor-neutral agent-runtime concepts (permission modes, checkpoints, capabilities, MCP servers) |
| `examples/stdin_demo.rs` | Full `Server` wiring + 5s KG-size ticker + Ctrl-C dump |

## Conventions

- Triples are **append-only with timestamps** — repeated `(S, P, O)` observations are intentionally kept (see `kg::tests::temporal_duplicates_are_kept`), not deduplicated.
- Extraction failures are logged and swallowed (return empty `Vec`), never propagated — a bad line must not abort the worker.
- Entity/predicate strings come straight from the LLM lowercased by prompt rule; there is no canonicalization layer yet. Don't assume `"alice" == "Alice Smith"`.
- Predicate IRIs minted by `src/iri.rs` use the `urn:ontrace:p/<local-name>` namespace as a fallback. Curated ontologies (seed packs in `ontologies/`, or anything loaded via `OxigraphPlugin::with_ontology_file`) can use any IRI scheme — the extractor only sees local names (the part after the last `#`, `/`, or `:`). When the LLM emits `met` and the TBox declares `<urn:ontrace:onto/agent/met>` as an `owl:ObjectProperty`, the write path bridges the bare local name to that curated IRI via `OntologyContext::predicate_iris` (`src/agents/extraction.rs`), so the `ABox` quad and `TBox` declaration share an IRI. Only predicates the LLM coins outside the curated set fall back to the `urn:ontrace:p/<local-name>` minting.
- Seed-pack defaults are closed-world: `SeedOntology::load` / `from_string` set `OntologyPolicy::Strict` — a line that fits no curated predicate yields zero triples. `OxigraphExtractionPlugin::new()` also defaults to `OntologyPolicy::Strict`. Chain `.with_policy(OntologyPolicy::CatchAll { … })` to opt into an open-world sentinel; there is no bundled catch-all predicate. The `predicate_iris` bridge is built once per session: the naive seed loader collects `(local-name, full-iri)` pairs while parsing the Turtle with `oxttl` (`parse_seed` in `src/agents/seed.rs`); the oxigraph snapshot collects the same map via SPARQL against the `TBox` graph (`query_predicate_iris` in `src/agents/oxigraph_extraction.rs`). The `OntologyPolicy::CatchAll::catchall_iri` field bridges a caller-supplied sentinel to its `TBox` IRI; the general map covers every curated predicate.
