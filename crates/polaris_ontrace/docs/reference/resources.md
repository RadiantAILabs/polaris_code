# Resource Documentation Standard

A **resource** in `polaris_ontrace` is a value a `#[system]` reads through a
typed parameter — `Res<KnowledgeGraph>`, `Res<TraceLine>`, `Res<SeedOntology>`.
Globals are server-wide and durable; locals are scoped to a single session.
Because systems are wired by their in/out type signatures, a resource's doc is
read by a system author deciding what to put in a parameter list — so it must
state the scope, who registers it, how it may be accessed and mutated, and what
the related variants are. This page is the normative standard for resource docs
in this crate.

## Documentation Standard

A resource type's rustdoc MUST open from a **system author's perspective**: "use
this when your system needs to X" (e.g. "use `Res<KnowledgeGraph>` when your
system appends extracted triples to the shared graph"). Past that, the following
sections apply.

### Required sections

Flag an ISSUE if any of these is missing.

| Section | Content |
|---------|---------|
| Purpose + when-to-use | From the consuming system's point of view. |
| `# Scope` | `Global` or `Local`, plus one sentence justifying the choice. |
| `# Provided by` | The plugin that registers it (with feature gate if any), or "consumer-supplied". |
| `# Access` | Valid param types (`Res<T>` / `ResMut<T>`) and the mutation contract. For globals, name the interior-mutability mechanism (e.g. `parking_lot::RwLock`, `Arc`-shared store). |
| `# Alternatives` | Related or variant resources a system author might pick instead, or `_none_`. |
| `# Example system` | A real-shape `#[system]` consuming it. |

### Conditional sections

Flag an ISSUE only when the trigger applies and the section is missing.

| Section | Trigger |
|---------|---------|
| `# Serialization` | The type impls `Storable` or is checkpointed/persisted. |
| `# Lifecycle hooks` | Its value semantics change across hook boundaries (reset, snapshot, swap). |

## Canonical Exemplars

- **`KnowledgeGraph`** (`crate::KnowledgeGraph`, `src/resources/kg.rs`) — the
  core **Global**. Append-only temporal triple store accessed through
  `Res<KnowledgeGraph>` and mutated via interior `parking_lot::RwLock` (there is
  no `ResMut` for globals). The exemplar for documenting the
  interior-mutability contract and "why Global" (durable state shared across
  every per-trace session).
- **`OxigraphStore`** (`crate::oxigraph::OxigraphStore`, feature `oxigraph`) —
  **Global** alternative backend to `KnowledgeGraph`. Cheaply clonable; the
  inner `oxigraph::store::Store` is `Arc`-shared so all handles see the same
  dataset. The exemplar for a strong `# Alternatives` cross-link (a system
  author picks `KnowledgeGraph` for the in-memory path or `OxigraphStore` for
  the SPARQL-queryable RDF path — never both in one server).
- **`TraceLine`** (`crate::TraceLine`, `src/resources/trace_line.rs`) — per-trace
  **Local** input. Inserted into each session's context by the worker
  (`ctx.insert(line)`) and read via `Res<TraceLine>`. The exemplar for a
  short-lived, per-session input whose `# Scope` is justified by "rebuilt fresh
  per trace, never shared."
- **`SeedOntology`** (`crate::SeedOntology`, `src/agents/seed.rs`) — **Local**,
  **consumer-supplied**. The consumer loads a Turtle pack via
  `SeedOntology::load` / `load_dir` and attaches it with `.with_seed(...)`; the
  plugin clones it into each session and the agent's `setup` reads it. The
  exemplar for "consumer-supplied" in `# Provided by` and for documenting that
  absence is valid (no seed → free-form extraction).

## Anti-Patterns

- **Smuggling system-to-system data through `ResMut`.** Inter-system flow is
  `Out<T>` (here `Out<Vec<RawTriple>>` carries `extract_triples` →
  `write_to_kg`). A resource is durable or input state, not a mailbox between
  two systems in the same graph.
- **Exposing internal collections as a `GlobalResource`.** Keep the inner index
  structures (the `by_subject` / `by_predicate` maps inside `KnowledgeGraph`)
  non-`pub` or `#[doc(hidden)]`; the resource's public surface is the contracted
  one, not its storage layout.
- **Documenting the type but not the contract.** Stating "an in-memory graph"
  without saying it is append-only, interior-mutable, accessed via `Res<T>`
  only, and registered by `ExtractionPlugin`. The contract — scope, access,
  mutation, provider — is the part a system author actually needs.
