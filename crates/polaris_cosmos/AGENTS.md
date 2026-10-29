# AGENTS.md — polaris_cosmos

Guidance for coding agents working in this crate. `CLAUDE.md` is a symlink to
this file.

## What this is

`polaris_cosmos` is the domain-neutral **cognitive-model library** and
**world-model substrate** for Polaris agents.

- [Polaris](https://github.com/RadiantAILabs/polaris) (`polaris-ai`) owns graph
  mechanics: systems, graphs, signatures, execution, resources, hooks, and
  lifecycle.
- `polaris_cosmos` owns reusable cognitive logic expressed with those
  primitives: reasoning models, memory (`W`), recall, consolidation, rewire,
  credit, and learning policies.
- Embodiment crates such as `crates/polaris_code_core` supply domain actions,
  observations, tools, verifiers, and environment resources.

A self-improving agent remains `M = (G, W)`: a graph schema `G` coupled to
memory `W`. Cosmos may ship many schemas; no cognition model is canonical.
Each model is one named point that agent builders may select, compose, or
replace.

It is a **library crate**, not a binary. Cognitive models are ordinary Polaris
graph builders. Their derived `GraphSignature` is the structural contract;
shared payload types, capability names, role schedules, documentation, and
behavioral tests supply semantics. Optional long-lived surfaces may be packaged
as `polaris::Plugin`s when lifecycle integration is needed.

**Rust edition:** 2024 (MSRV 1.97.0) · **License:** Apache-2.0 · `publish = false`

Read [`docs/taxonomy.md`](docs/taxonomy.md) for the ownership boundary, the
`M = (G, W)` model, and the cognitive-model layers.

## Status

The crate provides:

- Cognitive models in `cognition`: `prediction_error_loop`, plus the
  `coverage_sweep` and `bounded_correction` points in `cognition::approved`.
  Agent builders supply the typed stage systems and policies; Cosmos supplies
  the topology and semantic role schedules.
- Topology discovery in `topology`: stable identities and semantic contracts,
  validated `RegisteredTopology` entries, admission checks, and a
  startup-sealed `TopologyCatalog` global resource with immutable snapshots.
  `TopologyCatalogPlugin` supplies its two-phase lifecycle, while topology-pack
  plugins contribute registrations through `Extends<TopologyCatalog>`.
- Shared values in `contracts`: deterministic-CBOR records for identities,
  profiles, assessments, availability, selection, runtime evidence, and
  composition decisions.
- Selection in `selection`: replaceable, explained policy points that choose
  among catalog candidates.
- Composition in `composition`: gate plans, a block catalogue, limits, effect
  checks, and a materializer that builds native graphs.
- Evidence in `evidence`: an append-only store, contextual statistics, gate-plan
  outcome indexing, and `TopologyEvidencePlugin`.

This crate does not provide a world-model backend, durable memory
implementations, credit, or rewire, and it publishes no `API`s.

## Critical: single `polaris-ai`

Every crate in a consumer's graph must resolve to **one** `polaris-ai` source, or
shared types (`Server`, `Plugin`, `Res`, `Out`, …) won't unify and you get
`expected struct polaris_ai::X, found polaris_ai::X` errors.

- This crate takes Polaris from the workspace: `polaris.workspace = true`
  resolves to the single `[workspace.dependencies] polaris` dependency in
  the root `Cargo.toml`. Do not add a separate `polaris-ai` requirement here.
- Any other crate in the workspace that needs Polaris must use the same
  workspace entry, so every crate shares one package instance.
- If an external consumer depends on this crate, it must resolve `polaris-ai`
  to the same version as this crate does.

After any dependency change, run `cargo tree -i polaris-ai`; it must show
exactly one `polaris-ai` root.

## Build / test

From the workspace root:

```sh
cargo check -p polaris_cosmos
cargo test -p polaris_cosmos
cargo fmt --all -- --check
cargo clippy -p polaris_cosmos --all-targets -- -D warnings
cargo tree -i polaris-ai
```

Clippy is a wall: `-D warnings` plus `missing_docs = "warn"` and the
`doc_markdown` / `semicolon_if_nothing_returned` lints in `Cargo.toml` mean every
public item needs a doc comment. `cargo check` alone is not sufficient before
committing.

## Layout

```text
src/
  cognition.rs                    # shared cognitive roles + model modules
  cognition/
    prediction_error_loop.rs      # domain-neutral predict/act/verify/compare loop
    approved.rs                   # coverage_sweep and bounded_correction points
  contracts/                      # shared CBOR value records (assessment,
                                  #   availability, composition, evidence,
                                  #   identity, profile, runtime, selection)
  topology.rs                     # topology-discovery public surface
  topology/
    identity.rs                   # stable topology/contract/registration IDs
    descriptor.rs                 # semantic contracts + descriptors
    catalog.rs                    # validated registrations + sealed snapshots
    admission.rs                  # type explanations, preflight, start permits
    plugin.rs                     # catalog provider lifecycle
  selection.rs                    # selection policy points
  selection/
    deterministic.rs              # deterministic root selector
    transparent_profile.rs        # explained profile policy
  composition.rs                  # gate-plan composition public surface
  composition/
    plan.rs                       # gate plans and wiring
    catalogue.rs                  # block catalogue + sealed snapshots
    factory.rs, identity.rs       # block factories + instance identity
    description.rs                # discoverable block descriptions
    effects.rs                    # effect and repeat-safety checks
    limits.rs                     # host-supplied composition limits
    refusal.rs                    # planner-actionable refusals
    materializer.rs               # gate plan -> native graph
  evidence.rs                     # topology evidence public surface
  evidence/
    store.rs                      # evidence store boundary + in-memory store
    gate_plan.rs                  # gate-plan records and outcome index
    plugin.rs                     # `TopologyEvidencePlugin`
  lib.rs                          # crate root + discoverability catalogs
  docs/
    models.md                     # cognitive-model catalog
    plugins.md                    # plugin catalog
    apis.md                       # API catalog (currently empty)
    resources.md                  # resource catalog
docs/
  philosophy.md                   # shared-reference pointer
  taxonomy.md                     # ownership and cognitive-model layers
  reference/
    cognitive-models.md           # cognitive-model composition guide
    plugins.md                    # plugin documentation standard
    api.md                        # API documentation standard
    resources.md                  # resource documentation standard
    gate-plan-evidence.md         # gate-plan evidence records
    guide.md                      # downstream integration guide
tests/
  plugin_catalog.rs               # Plugin catalog drift guard
  api_catalog.rs                  # API catalog drift guard
  resource_catalog.rs             # Resource catalog drift guard
  cognition_models.rs             # cognitive-model behaviour
  contracts_*.rs                  # shared value records
  topology_*.rs                   # catalog, admission, plugin, pack authoring,
                                  #   and end-to-end runtime behaviour
  selection_deterministic.rs      # deterministic selection
  composition_materializer.rs     # gate plans -> native graphs
  block_description.rs            # block descriptions
  runtime_composition_contract.rs # runtime composition contract
  evidence_*.rs                   # evidence store and plugin
  gate_plan_evidence.rs           # gate-plan evidence records
  upstream_typed_system_erasure.rs # typed system erasure
  fixtures/                       # shared test fixtures
```

When adding a plugin, API, or resource, document it against the corresponding
standard under `docs/reference/` and add it to the matching `src/docs/`
catalog. When adding a cognitive model, add it to `src/docs/models.md` and
document its graph signature, semantic roles, embodiment ports, state
lifetime, policies, and behavioral tests.

## Conventions

- Document all public items (`missing_docs = "warn"`).
- Use `tracing`, never `print*`.
- A cognitive model is a **point**, not the model axis. Never present one
  topology as canonical intelligence.
- Build on Polaris `Graph`, `GraphSignature`, systems, resources, schedules,
  and dynamic slots; do not create a parallel executor or structural type
  system.
- Structural compatibility comes from graph signatures. Semantic compatibility
  comes from shared types, capability names, role schedules, documentation, and
  tests.
- Apply the embodiment test: logic meaningful across coding, robotics, research,
  or another domain belongs in Cosmos; code-, IDE-, compiler-, repository-, or
  tool-specific behavior belongs in `crates/polaris_code_core`.
- Every substrate or policy is a point on an axis, not the axis. Ship a trivial
  default and keep the slot open.
- Cognitive and world-model contracts have whole-system blast radius. Discuss
  framework-shaped changes before implementing them.
- Commits use one concise sentence, no body or trailers.
