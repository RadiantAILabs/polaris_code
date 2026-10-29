A catalog of every consumer-facing [`GlobalResource`](polaris::system::resource::GlobalResource)
and [`LocalResource`](polaris::system::resource::LocalResource) shipped by
`polaris-ontrace` — the types a `#[system]` consumes via `Res<T>`.

Each entry links to the resource's rustdoc page, which documents its scope,
provider, access pattern, and a real-shape consuming system per the
[resource documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_ontrace/docs/reference/resources.md#documentation-standard).

# Knowledge graph — durable state

The graph backends are mutually exclusive: register one extraction backend, get
one of these. Both are interior-mutable globals shared across every per-trace
session.

| Resource | Scope | Description |
|----------|-------|-------------|
| [`KnowledgeGraph`](crate::KnowledgeGraph) | Global | The in-memory temporal triple store. Append-only with timestamps — repeated `(S, P, O)` observations are kept, not deduped. Inserted by [`ExtractionPlugin`](crate::ExtractionPlugin). |
| [`OxigraphStore`](crate::oxigraph::OxigraphStore) *(feature `oxigraph`)* | Global | The RDF/SPARQL-backed alternative: a shared `oxigraph` dataset supporting SPARQL, Turtle, and SHACL. Inserted by [`OxigraphPlugin`](crate::oxigraph::OxigraphPlugin); written by [`OxigraphExtractionPlugin`](crate::OxigraphExtractionPlugin). |

# Extraction bookkeeping — shared globals

Registered alongside the graph by every extraction backend; read by systems or
dumped at shutdown.

| Resource | Scope | Description |
|----------|-------|-------------|
| [`Aliases`](crate::Aliases) | Global | Entity canonicalization table — maps normalized surface forms to a canonical IRI and emits each `owl:sameAs` at most once. |
| [`Sightings`](crate::Sightings) | Global | Append-only log of every entity/predicate mention, for provenance and frequency. |
| [`Spend`](crate::Spend) | Global | Monotonic per-model accumulation of LLM call counts and input/output token usage. |
| [`OntologyGap`](crate::OntologyGap) | Global | Append-only record of SHACL violations / extractions that fit no curated shape — the signal that drives ontology repair. |

# Per-trace input & seed vocabulary — locals

| Resource | Scope | Description |
|----------|-------|-------------|
| [`TraceLine`](crate::TraceLine) | Local | The single trace under extraction. Built fresh per session by the worker from an [`IOMessage`](polaris::plugins::IOMessage); read-only input. |
| [`SeedOntology`](crate::SeedOntology) | Local | A curated starter vocabulary loaded from `.ttl` files. Consumer-supplied via `ExtractionPlugin::with_seed`; constrains the naive backend to closed-world (Strict) extraction. |
