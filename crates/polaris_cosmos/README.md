# polaris_cosmos

Domain-neutral cognition and world-model infrastructure for
[Polaris](https://github.com/RadiantAILabs/polaris) agents.

Polaris supplies the graph runtime. Cosmos supplies a library of cognitive
models expressed as Polaris graphs, plus the memory side of the self-improving
agent model `M = (G, W)`. Agent builders choose a cognitive model and embody it
with domain-specific actions, observations, tools, verifiers, and resources.

No cognition model is canonical. Every model is a named point with a derived
`GraphSignature`, so builders can inspect its structural interface and compose
or replace compatible graphs using native Polaris mechanisms.

## Cognitive models

The library includes
[`cognition::prediction_error_loop`](src/cognition/prediction_error_loop.rs),
systematic [`coverage_sweep`](src/cognition/approved.rs), and bounded iterative
correction:

```text
predict → act → verify → compare → gate ─┬─ accept
                                           └─ reject
```

Cosmos owns these domain-neutral logical topologies, bounds, accounting rules,
and role schedules. An embodiment supplies typed systems for concrete actions,
observations, verifiers, and result semantics. Direct routed graphs remain an
embodiment pattern rather than a universal Cosmos builder.

## Topology discovery

[`topology::TopologyCatalog`](src/topology/catalog.rs) is the first discovery
kernel. Applications register owned Polaris graphs against stable semantic
contracts; the catalog runs native graph validation, derives each
`GraphSignature`, checks claimed contracts by subsumption, and then seals into
an immutable selection snapshot. Catalog snapshots expose descriptors and
signatures, while the live catalog retains executable graph handles for later
resolution. `TopologyCatalogPlugin` provides the default lifecycle: topology
packs extend the mutable catalog during plugin construction, after which the
provider seals and globalizes it for execution.

## Selection and evidence

The host-neutral topology runtime now includes:

- transparent, provenance-preserving task-profile aggregation;
- complete mechanical preflight plus live native readmission and a one-use
  start permit;
- deterministic objective-weighted selection with full explanations,
  exclusions, fallback order, and typed no-eligible outcomes; and
- a replaceable append-only evidence provider with bounded in-memory retention,
  delayed/conflicting outcome observations, and content-addressed contextual
  statistics.

The host still resolves bindings, constructs pristine execution contexts,
starts the selected native Polaris graph, and decides what to do after side
effects. See the [integration guide](docs/reference/guide.md) for the full
selection → admission → execution → outcome → statistics sequence.

## World model

Cosmos also owns memory (`W`) and the domain-neutral logic coupling it to graph
schemas (`G`): recall, consolidate, rewire, and credit assignment. This crate
does not yet include a world-model backend contract or a durable implementation.

## Build and test

From the workspace root:

```sh
cargo check -p polaris_cosmos
cargo test -p polaris_cosmos
cargo fmt --all -- --check
cargo clippy -p polaris_cosmos --all-targets -- -D warnings
cargo tree -i polaris-ai
```

## Where to look

- [`docs/reference/cognitive-models.md`](docs/reference/cognitive-models.md) —
  composing and embodying Cosmos models.
- [`docs/reference/guide.md`](docs/reference/guide.md) — integrating catalog,
  admission, selection, execution, and evidence without moving host ownership
  into Cosmos.
- [`docs/taxonomy.md`](docs/taxonomy.md) — ownership boundaries, `M = (G, W)`,
  and the crate's layers.
- [`src/docs/models.md`](src/docs/models.md) — cognitive-model catalog.
- [`src/docs/plugins.md`](src/docs/plugins.md) — plugin catalog, including the
  topology catalog provider.
- [`src/docs/resources.md`](src/docs/resources.md) — consumer-facing resource
  catalog, including `TopologyCatalog`.
- [`docs/philosophy.md`](docs/philosophy.md) — shared Polaris design
  philosophy.
- [`AGENTS.md`](AGENTS.md) — repository guidance for coding agents.
