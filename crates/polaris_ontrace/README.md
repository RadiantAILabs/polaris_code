# polaris_ontrace

A [Polaris](https://github.com/RadiantAILabs/polaris) plugin that tails a
trace stream, runs a per-trace LLM extraction agent on each line, and
accumulates `(subject, predicate, object, timestamp)` triples into a
**temporal knowledge graph**. Extraction is shaped by a curated seed
ontology (see [Ontology-aware extraction](#ontology-aware-extraction)),
so the LLM emits predicates from a closed vocabulary instead of
free-form text.

## Quick start

1. Put your Anthropic key in a `.env` file in the repo (or any parent dir):

    ```sh
    ANTHROPIC_API_KEY=sk-ant-...
    ```

2. Tail your newest Claude Code session, with the bundled feedback-loop
   ontology guiding extraction:

    ```sh
    cargo run --bin ontrace -- tail --seed-dir ontologies --packs core,llm,agent
    ```

    `ontrace tail` picks the newest session under `~/.claude/projects/`,
    extracts triples constrained to the merged `core + llm + agent`
    vocabulary, and snapshots the KG to `./data/<timestamp>.ttl` every
    5 seconds (and once more on Ctrl-C).

3. Other common entry points:

    ```sh
    cargo run --bin ontrace -- list                                # list known Claude Code sessions
    cargo run --bin ontrace -- tail <session-id> --seed ontologies/agent.ttl
    cargo run --bin ontrace -- tail --stdin --seed-dir ontologies   # pipe lines on stdin
    cargo run --bin ontrace -- tail --file path/to/traces.jsonl
    cargo run --bin ontrace -- tail --otlp                          # accept OTLP/HTTP on :4318

    # Inspect a saved KG with a SPARQL-tool-equipped agent (oxigraph is on by default).
    # The positional arg is a .ttl file or a directory of .ttl files:
    cargo run --bin ontrace -- \
        inspect data/<timestamp>.ttl --seed-dir ontologies
    cargo run --bin ontrace -- \
        inspect data/<timestamp>.ttl --ask "what are the top 5 predicates by count?"

    # Run raw SPARQL against a saved KG — no LLM, no API key:
    cargo run --bin ontrace -- query data/ --seed-dir ontologies    # REPL; dir loads every .ttl
    ```

The bare `ontrace` / `ontrace <session-id>` forms still work as shortcuts
for `ontrace tail`. `cargo run --bin ontrace -- --help` lists the
subcommands; add `--help` after any subcommand for its flags. See
[Ontology-aware extraction](#ontology-aware-extraction) for what `--seed`
/ `--seed-dir` / `--packs` actually do.

## How it runs

Two KG backends ship in-tree: a full RDF oxigraph store split across an
ontology (`TBox`) and data (`ABox`) named graph (default — gated on the
`oxigraph` cargo feature, which is in the default feature set) and a
lightweight naive in-memory `KnowledgeGraph` (always compiled in,
selected via `--backend memory` or by dropping the default feature with
`--no-default-features`). Both are exposed as Polaris global resources
and driven by the same source plugins.

Each run writes its KG to `<out>/<timestamp>.ttl` (default `./data/`),
overwriting the same file on every 5-second snapshot — consecutive runs
never clobber each other since each gets its own timestamp. Override the
directory with `ontrace tail --out <dir>`.

### Minimal library example

For embedding the pipeline in your own binary, `examples/stdin_demo.rs`
shows the smallest possible wiring (`PersistencePlugin` + `ModelsPlugin`
+ `AnthropicPlugin` + `SessionsPlugin` + `TraceIngestPlugin` +
`ExtractionPlugin` + `StdinIOPlugin`, no seed):

```sh
printf 'alice met bob in new york\nbob works at acme corporation\n' \
    | cargo run --example stdin_demo

env RUST_LOG=polaris_ontrace=trace,polaris=info cargo run --example stdin_demo
```

The demo blocks until Ctrl-C — stdin EOF stops new lines from arriving
but the server stays alive so you can keep watching the KG-size ticker.
For seeded / multi-source / oxigraph deployments, use the `ontrace`
binary above; the example stays bare on purpose.

### Trace sources

Ontrace splits cleanly into three pieces: a `TraceIngestPlugin` that owns
the fan-in channel, an extraction backend (`ExtractionPlugin` or
`OxigraphExtractionPlugin`) that drains it, and a family of source
plugins that feed it. Pick whichever combination matches the deployment.

| Plugin | What it does |
|--------|--------------|
| `StdinIOPlugin` | Read newline-delimited lines from `stdin`. |
| `FileTailIOPlugin(path)` | `tail -F` semantics over a JSONL/log file. |
| `OtlpIOPlugin` | Accept OTLP/HTTP traces and logs (JSON) on `127.0.0.1:4318`. |
| `ClaudeCodeIOPlugin` | Convenience wrapper for Claude Code session files (newest/by-id/explicit-path) and `stream-json` stdin. |

Source plugins can be composed — `--stdin --otlp` runs both transports
in parallel feeding the same extractor:

```sh
cargo run --bin ontrace -- tail --stdin --otlp

# Then point an OTel SDK at http://127.0.0.1:4318/v1/traces (JSON encoding).
```

The OTLP receiver flattens each `Span` and `LogRecord` into one
`IOMessage::Structured` carrying the merged
`{resource, scope, span | log}` payload, then hands it to the extractor —
the same prompt path as stdin/file lines.

### Backends

`ontrace tail --backend <kind>` picks the KG storage layer. Source plugins
are agnostic — stdin, file tail, OTLP, and Claude Code all feed either
backend unchanged.

| `--backend` value | Storage | Required feature |
|---|---|---|
| `oxigraph` *(default)* | In-memory oxigraph RDF store, split into `urn:polaris:graph:ontology` (`TBox`) and `urn:polaris:graph:data` (`ABox`) named graphs. | `oxigraph` (in default set) |
| `oxigraph:<dir>` | RocksDB-backed oxigraph store at `<dir>` — durable across restarts. | `oxigraph-rocksdb` |
| `memory` | Naive in-memory `Vec<Triple>` + indexes. Periodic snapshot is the Turtle dump described below. | — |

```sh
# In-memory oxigraph is the default backend — no flag needed.
cargo run --bin ontrace -- tail --stdin

# Durable rocksdb-backed store (first build pulls in ~30s of C++ compile)
cargo run --bin ontrace --features oxigraph-rocksdb -- \
    tail --backend oxigraph:./kg.db --stdin

# Naive in-memory backend (skip oxigraph compile cost or stay on the Vec<Triple> path):
cargo run --bin ontrace --no-default-features -- tail --stdin
# …or, with oxigraph compiled in, opt down at runtime:
cargo run --bin ontrace -- tail --backend memory --stdin
```

Under the oxigraph backends, the extractor writes into the data graph
and the periodic snapshot dumps only that graph (the ontology graph is
curated content that rarely changes). Load TBox content either at
startup via `OxigraphPlugin::with_ontology_file("schema.ttl")` or at
runtime via the SPARQL toolset's `load_ontology_turtle` tool — see
`src/oxigraph/tools.rs` for the toolset surface.

If the binary is built without the `oxigraph` feature (e.g.
`--no-default-features`), the default backend becomes `memory` and
`--backend oxigraph` exits with a clear error rather than silently
falling back.

### Ontology-aware extraction

Both backends share the same extraction step (`extract_triples`) and the
same `OntologyContext` local resource. When curated content is present
the extractor snapshots it once per session and uses it to shape the
LLM call:

- The curated predicate list is injected into the system prompt as a
  guidance summary.
- The JSON schema's `predicate` field is constrained to a closed `enum`
  over those predicates, so the model cannot emit anything outside the
  curated vocabulary.

Where the curated content comes from differs by backend:

| Backend | Source |
|---|---|
| Naive in-memory | A `SeedOntology` inserted per-session by `ExtractionPlugin::with_seed(...)`. |
| Oxigraph | A SPARQL `SELECT` over the TBox named graph — populated either by `OxigraphPlugin::with_ontology_file("schema.ttl")`, `OxigraphExtractionPlugin::with_seed(...)`, or the `load_ontology_turtle` SPARQL tool at runtime. |

The escape hatch is controlled by `OntologyPolicy`:

| Policy | Behavior |
|---|---|
| `OntologyPolicy::Strict` | The schema enum is exactly the curated predicates. Lines whose relations don't fit any curated predicate yield zero triples — the LLM is instructed to return an empty `triples` array when nothing matches. |
| `OntologyPolicy::CatchAll { catchall_predicate }` | The schema enum is `curated predicates ∪ {catchall}`. The LLM uses a caller-chosen sentinel predicate for facts that don't fit a curated shape, so observations aren't lost — downstream consumers can route or quarantine catchall triples by predicate. |

**Policy defaults** are uniform across constructors, so they don't accidentally drift between backends:

- `SeedOntology::load(path)`, `SeedOntology::load_dir(dir)`, `SeedOntology::from_string(...)`: **Strict** (closed-world — a line that fits no curated predicate yields zero triples).
- `OxigraphExtractionPlugin::new()`: **Strict**. `.with_seed(seed)` adopts `seed.policy`.
- Either constructor's `.with_policy(p)` is last-wins — pass `OntologyPolicy::CatchAll { … }` to opt into an open-world sentinel.

#### Seed packs (`ontologies/`)

The repo ships a small layered ontology under `ontologies/` (importable
into any oxigraph store, and parsed for the schema enum on either
backend):

| Pack | Imports | Covers |
|---|---|---|
| `core` | — | `Actor`, `Artifact`, `Event`, `Measurement`, `Resource`, `Failure`, `Resolution` + `produced` / `consumed` / `errored_with` / `caused_by` / `part_of` / `child_of` / `resolved_by` / `superseded_by` |
| `llm` | `core` | providers, models, requests, responses, token usage (with scalar counts), latency, rate limits, `stop_reason` |
| `agent` | `core`, `llm` | agents, tools, tool calls (with `exit_code`), plans, objectives |
| `harness` | `core`, `agent` | permission modes, checkpoints, capabilities (skills, slash commands, hooks), MCP servers |
| `user` | `core`, `agent` | user role, preferences, intentions, environment, feedback, session titles |
| `coding` | `core`, `agent` | files, modules, functions, commits, PRs, branches, design patterns |
| `eval` | `core`, `agent` | eval runs, datasets, judges, rubrics, metrics |
| `tracing` | `core`, `llm` | distributed-trace spans |

Typical compositions: feedback-loop = `core + llm + agent`;
coding-agent = `+ coding`; agent-harness traces = `+ harness`; eval run
= `+ eval`; LLM observability = `core + llm + tracing`. Use
`SeedOntology::merged` to union layers into a single seed.

The loader is **path-driven** — paths are never baked into the library.
The `--seed <path>` / `--seed-dir <dir>` / `--packs a,b,c` flags are shared
by the `tail`, `inspect`, and `query` subcommands:

```sh
cargo run --bin ontrace -- tail --seed-dir ontologies --packs core,llm,agent
cargo run --bin ontrace -- tail --seed ontologies/agent.ttl
```

From your own binary, the equivalent calls are:

```rust
use polaris_ontrace::{ExtractionPlugin, OntologyPolicy, SeedOntology};

// Naive in-memory backend, agent-loop pack:
let packs = SeedOntology::load_dir("ontologies")?;
let seed = SeedOntology::merged(
    "agent_loop",
    packs.into_iter().filter(|p| ["core", "llm", "agent"].contains(&p.name.as_str())),
);

server.add_plugins(ExtractionPlugin::new().with_seed(seed));
```

```rust
// Oxigraph backend — `.with_seed` also loads the seed's Turtle into the TBox.
server
    .add_plugins(OxigraphPlugin::in_memory())
    .add_plugins(
        OxigraphExtractionPlugin::new()
            .with_seed(SeedOntology::load("ontologies/agent.ttl")?),
    );
```

With no seed and no TBox content, both backends collapse to free-form
extraction — same prompt, unconstrained predicate field.

### Dump format

The snapshot file is standard RDF Turtle, ready to load into oxigraph,
Apache Jena, or query with SPARQL. Each observation appears in two forms:

```turtle
@prefix ont: <urn:ontrace:> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .

# Direct triples — deduplicated for clean SPARQL queries
<urn:ontrace:e/alice> <urn:ontrace:p/met> <urn:ontrace:e/bob> .

# Reified observations — preserve full temporal history
_:obs0 a rdf:Statement ;
    rdf:subject <urn:ontrace:e/alice> ;
    rdf:predicate <urn:ontrace:p/met> ;
    rdf:object <urn:ontrace:e/bob> ;
    ont:observedAt "2026-05-12T09:54:04+00:00"^^xsd:dateTime .
```

Entity and predicate names are percent-encoded so arbitrary text (spaces,
punctuation) round-trips cleanly.

Curated predicates declared in a loaded `TBox` land at their declared IRI,
not the `urn:ontrace:p/` fallback. With `--seed-dir ontologies/` and the
`agent` pack, `(alice, met, bob)` becomes:

```turtle
<urn:ontrace:e/alice> <urn:ontrace:onto/agent/met> <urn:ontrace:e/bob> .
```

so `?s <urn:ontrace:onto/agent/met> ?o` joins to the `TBox` declaration
directly. Predicates the LLM coins outside the curated set (only possible
under `CatchAll`) still mint to `urn:ontrace:p/<local-name>`.

### Inspect mode

`ontrace inspect <ttl-or-dir>` (oxigraph feature) loads a saved KG into a
fresh in-memory oxigraph store, exposes the `OxigraphTools` SPARQL toolset
(`sparql_query`, `sparql_update`, `load_ontology_turtle`,
`describe_resource`) with permissions widened to `Allow`, and opens the
`polaris-tui` shell with four panels:

- **Inspect** — chat-style transcript driven by `InspectAgent` (the agent
  loops the LLM against the toolset until it stops emitting tool calls,
  then renders the final answer in-line).
- **SPARQL** — multi-line editor + result pane that runs raw queries
  against the same oxigraph store on a blocking worker. `Enter` submits;
  `Shift-Enter` / `Ctrl-J` insert a newline. Results render as Turtle
  (`CONSTRUCT` / `DESCRIBE`) or SPARQL Results JSON (`SELECT` / `ASK`) —
  the same forms `inspect`'s `sparql_query` tool returns, so a query
  prototyped in this panel transfers straight into the chat.
- **Logs** — tail of `tracing` events from the running process.
- **Help** — global keymap (`Tab` cycles, `Ctrl-L` tiles ↔ single,
  `Ctrl-C` / `Ctrl-Q` quits).

The positional argument is a single `.ttl` file or a directory whose every
`.ttl` is loaded into the data graph; `--seed` / `--seed-dir` / `--packs`
load a curated ontology into the `TBox` alongside it.

```sh
# Open the shell on a saved KG, optionally with the curated ontology loaded into the TBox:
cargo run --bin ontrace -- \
    inspect data/<timestamp>.ttl --seed-dir ontologies --packs core,llm,agent

# Single-question mode (no TUI — pipes the answer to stdout):
cargo run --bin ontrace -- \
    inspect data/<timestamp>.ttl --ask "which agents called the most tools?"
```

The agent is briefed up front with the curated `TBox` (predicates,
classes, `Domain → Range` shapes) when one is loaded — see
`src/agents/inspect.rs::InspectOntologyOverview` for the format. With no
seed, the agent runs in pure discovery mode and can still SPARQL the
`TBox` graph itself.

Tool results are capped at 32 KiB per call — the system prompt steers the
agent toward summarizing queries (`COUNT`, `GROUP BY`, `LIMIT`, sampling)
rather than raw enumeration, so long sessions stay within the LLM's
context budget. Use `--model <id>` to override the default
(`anthropic/claude-sonnet-4-6` — stronger tool-using model than the
extractor's Haiku default).

### Query mode

`ontrace query <ttl-or-dir> [SPARQL]` (oxigraph feature) is the
non-agentic counterpart to `inspect`: it builds the same store layout — a
`.ttl` file or directory loaded into the data graph, optional `--seed` /
`--seed-dir` / `--packs` into the `TBox` — but runs raw SPARQL directly,
with **no LLM and no `ANTHROPIC_API_KEY`** required.

With a positional query string it runs once and prints the result; without
one it opens a one-query-per-line stdin REPL. `SELECT` / `ASK` results
serialize as SPARQL Results JSON, `CONSTRUCT` / `DESCRIBE` as Turtle — the
same forms `inspect`'s `sparql_query` tool returns, so a query prototyped
here transfers straight into an inspect session.

```sh
cargo run --bin ontrace -- query data/<timestamp>.ttl \
    "SELECT ?p (COUNT(*) AS ?n) WHERE { GRAPH <urn:polaris:graph:data> { ?s ?p ?o } } GROUP BY ?p ORDER BY DESC(?n)"

# REPL; a directory loads every .ttl into the data graph:
cargo run --bin ontrace -- query data/ --seed-dir ontologies
```

### Build / test / lint

From the workspace root:

```sh
# Default feature set (includes oxigraph)
cargo check -p polaris_ontrace --all-targets
cargo test -p polaris_ontrace
cargo clippy -p polaris_ontrace --all-targets -- -D warnings

# Naive-only gates (oxigraph compiled out)
cargo check  -p polaris_ontrace --no-default-features --all-targets
cargo test   -p polaris_ontrace --no-default-features
cargo clippy -p polaris_ontrace --no-default-features --all-targets -- -D warnings
```

Both feature configurations are gated on `-D warnings` plus
`missing_docs = "warn"`, so every public item needs a doc comment.

## Architecture

```text
  StdinIOPlugin     ──┐    TraceIngestPlugin
  FileTailIOPlugin  ──┼──► ┌────────────────────┐
  OtlpIOPlugin      ──┤    │  TraceIngestAPI    │
  ClaudeCodeIOPlugin──┘    │    push(IOMessage) │
                           │       │            │
                           │       ▼            │      ExtractionPlugin
                           │  mpsc<IOMessage>   │         OR
                           │       │            │      OxigraphExtractionPlugin
                           │       ▼            │        (one, never both)
                           │  TraceIngestRx ────┼──► take ─► worker
                           └────────────────────┘             │
                                                              ▼
                                           sessions.run_oneshot(<Agent>,
                                                |ctx| ctx.insert(TraceLine))
                                                              │
                                                              ▼
                                          extract_triples → write_to_kg          (memory)
                                                            write_to_oxigraph    (oxigraph)
                                                              │
                                                              ▼
                                      KnowledgeGraph  ─OR─  OxigraphStore (data graph)
                                           (GlobalResource — pick at server-build time)
```

The three pieces are fully decoupled. `TraceIngestPlugin` is the only
plugin the source-side and backend-side share by name: source plugins
depend on it to push, backends depend on it to take the receiver.
Picking between backends is a single registration choice — both reuse
the same `extract_triples` LLM call and the same `Aliases`
canonicalization table. Registering two backends panics on the second
`TraceIngestRx::take()`.

Each trace runs through a fresh, ephemeral session. The agent itself carries
no state between traces — all durable state lives in shared global
resources. Triples are append-only with observation timestamps, so
repeated `(S, P, O)` observations build a temporal history rather than
collapsing into a single edge.

## Project layout

| File | What it holds |
|------|---------------|
| `src/resources/kg.rs` | `KnowledgeGraph` (GlobalResource, RwLock-backed), `Triple`, `Entity`, `Predicate` |
| `src/resources/aliases.rs` | `Aliases` (GlobalResource), `Resolution` — entity canonicalization + `owl:sameAs` |
| `src/resources/trace_line.rs` | `TraceLine` (LocalResource) |
| `src/agents/extraction.rs` | `ExtractionAgent`, `AgentConfig`, `AgentLlm`, `OntologyContext` (carries the `predicate_iris` bridge), `OntologyPolicy`, `extract_triples`, `write_to_kg`, `RawTriple` |
| `src/agents/seed.rs` | `SeedOntology` (LocalResource) + `load` / `load_dir` / `from_string` / `merged` — runtime loader over `.ttl` files |
| `src/agents/oxigraph_extraction.rs` | `OxigraphExtractionAgent`, `snapshot_ontology`, `snapshot_curated`, `write_to_oxigraph` (feature: `oxigraph`) |
| `src/agents/inspect.rs` | `InspectAgent`, `InspectInput`, `InspectConfig`, `InspectOntologyOverview` — interactive SPARQL-tool agent over a loaded KG (feature: `oxigraph`) |
| `src/plugins/trace_ingest.rs` | `TraceIngestPlugin`, `TraceIngestAPI` (sources push), `TraceIngestRx` (backend takes) — the mpsc dispatcher |
| `src/plugins/extraction.rs` | `ExtractionPlugin`, `with_seed` — in-memory backend; claims `TraceIngestRx` and runs the worker |
| `src/plugins/oxigraph_extraction.rs` | `OxigraphExtractionPlugin`, `with_policy` / `with_seed` — oxigraph backend (feature: `oxigraph`) |
| `src/plugins/inspect.rs` | `InspectPlugin`, `InspectAPI` — registers the inspect agent and publishes `ask(question)` (feature: `oxigraph`) |
| `src/tui/` | `InspectTuiPlugin`, `InspectChatPanel`, `SparqlPanel` — polaris-tui panels that drive `InspectAPI::ask` and raw SPARQL against the shared `OxigraphStore` (feature: `oxigraph`) |
| `ontologies/*.ttl` | Layered starter packs (`core`, `llm`, `agent`, `harness`, `user`, `coding`, `eval`, `tracing`); loaded at runtime via `SeedOntology::load_dir` — consumers may keep their own directory elsewhere |
| `src/oxigraph/` | `OxigraphStore`, `OxigraphPlugin`, `OxigraphSession`, `OxigraphTools` SPARQL toolset (feature: `oxigraph`) |
| `src/sources/stdin.rs` | `StdinIOPlugin` + `StdinProvider` (`IOProvider`) |
| `src/sources/file_tail.rs` | `FileTailIOPlugin` + `FileTailProvider` — `tail -F` semantics |
| `src/sources/otlp.rs` | `OtlpIOPlugin` — OTLP/HTTP receiver on `axum` |
| `src/sources/claude_code.rs` | `ClaudeCodeIOPlugin` — session discovery + transport selection |
| `src/iri.rs` | Shared IRI minting (`urn:ontrace:e/…`, `urn:ontrace:p/…`) used by dump + oxigraph agent |
| `src/dump.rs` | `write_turtle(&KnowledgeGraph, &mut impl Write)` — RDF Turtle serializer for the memory backend |
| `src/bin/ontrace.rs` | CLI: `tail` / `inspect` / `query` / `list` subcommands (clap-derived); `tail` owns the source + `--backend` flags + periodic snapshot |
| `examples/stdin_demo.rs` | Wiring: `PersistencePlugin` + `SessionsPlugin` + `TraceIngestPlugin` + `ExtractionPlugin` + `StdinIOPlugin` |

## Dependencies (Polaris APIs used)

- `polaris::sessions::{SessionsAPI, SessionsPlugin}` — agent registration + `run_oneshot`
- `polaris::plugins::PersistencePlugin` — required by `SessionsPlugin`
- `polaris::sessions::store::memory::InMemoryStore` — session backing store
- `polaris::system::{plugin::Plugin, resource::{GlobalResource, LocalResource}, server::Server, system}`
- `polaris::agent::Agent`, `polaris::graph::Graph`, `polaris::prelude::Out`, `polaris::system::param::Res`

The `polaris` import name is the
[Polaris](https://github.com/RadiantAILabs/polaris) (`polaris-ai`) crate,
taken from the workspace's single `[workspace.dependencies] polaris`
dependency (`polaris-ai` from crates.io) and renamed via
`package = "polaris-ai"` there to keep `use polaris::...` paths short.

## Limitations

- **Lexical entity linking.** The `Aliases` table handles lexical
  normalization, curated seeds, and first-detection `owl:sameAs` emission.
  There is no embedding lookup, so `"alice"` / `"alice smith"` / `"user-1234"`
  are linked only by surface form.
- **One repair round.** `src/agents/shacl.rs` builds a per-session
  `ShapeIndex` from `rdfs:domain` / `rdfs:range` and SHACL property shapes in
  the `TBox`, and `validate_triples` runs between `extract_triples` and
  `write_to_oxigraph` to flag range-class / datatype / `sh:nodeKind`
  violations. `src/agents/repair.rs` sends flagged triples back to the LLM
  once. A triple that still fails is dropped, logged, and recorded into
  `OntologyGap`.
- **No ontology curation.** The `OxigraphExtractionAgent` writes only to the
  `ABox`. `TBox` edits come from the packs and files you load.
- **OTLP bodies are JSON only.** The OTLP receiver does not accept
  `application/x-protobuf` or OTLP/gRPC.
- **No served query surface.** `ontrace inspect` gives an LLM-driven REPL over
  `OxigraphTools` (`sparql_query`, `describe_resource`, …) and `ontrace query`
  a non-LLM SPARQL CLI, both over saved dumps. Nothing serves triples over
  HTTP.
- **One serial worker.** A single worker drains the channel, so each trace
  waits for the previous trace's Haiku call.

## License

Apache-2.0.
