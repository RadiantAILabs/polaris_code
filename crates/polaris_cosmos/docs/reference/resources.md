# Resource Documentation Standard

A **resource** in `polaris_cosmos` is a value a `#[system]` reads through a typed
parameter — `Res<W>`, `Res<CreditJoin>`, and the like. Globals are server-wide
and durable (the `W` substrate itself, the credit-join log); locals are scoped to
a single session. Because systems are wired by their in/out type signatures, a
resource's doc is read by a system author deciding what to put in a parameter
list — so it must state the scope, who registers it, how it may be accessed and
mutated, and what the related variants are. This page is the normative standard
for resource documentation in this crate.

## Documentation Standard

A resource type's rustdoc MUST open from a **system author's perspective**: "use
this when your system needs to X" (e.g. "use `Res<W>` when your system queries or
folds an observation into the world-model substrate"). Past that, the following
sections apply.

### Required sections

Flag an ISSUE if any of these is missing.

| Section | Content |
|---------|---------|
| Purpose + when-to-use | From the consuming system's point of view. |
| `# Scope` | `Global` or `Local`, plus one sentence justifying the choice. |
| `# Provided by` | The plugin that registers it (with feature gate if any), or "consumer-supplied". |
| `# Access` | Valid param types (`Res<T>` / `ResMut<T>`) and the mutation contract. For globals, name the interior-mutability mechanism (e.g. `parking_lot::RwLock`, `Arc`-shared store). |
| `# Alternatives` | Related or variant resources a system author might pick instead, or `_none_`. For a `W` backend, cross-link the other memory backends. |
| `# Example system` | A real-shape `#[system]` consuming it. |

### Conditional sections

Flag an ISSUE only when the trigger applies and the section is missing.

| Section | Trigger |
|---------|---------|
| `# Axis` | The resource *is* a point on an axis (a memory backend, a topology-selection policy). Name the axis and whether it is the trivial default. |
| `# Serialization` | The type impls `Storable` or is checkpointed/persisted (the snapshot+replay path depends on this). |
| `# Lifecycle hooks` | Its value semantics change across hook boundaries (reset, snapshot, swap). |

## Canonical Exemplars

> [`TopologyCatalog`](../../src/topology/catalog.rs) is the first in-repo
> exemplar: a startup-populated, sealed Global with no interior mutability. Its
> complete-registration path atomically retains native graphs, stable admission
> explanations, configuration identity, and author priors, while its snapshot
> digest is independent of pack insertion order. For
> an interior-mutable durable Global, follow `polaris_ontrace`'s
> `KnowledgeGraph`; use its `OxigraphStore` for an exemplar with a strong
> `# Alternatives` cross-link and `TraceLine` for a per-session Local input.

## Anti-Patterns

- **Smuggling system-to-system data through `ResMut`.** Inter-system flow is
  `Out<T>`; a resource is durable or input state, not a mailbox between two
  systems in the same graph.
- **Exposing internal collections as a `GlobalResource`.** Keep a backend's
  internal index structures non-`pub` or `#[doc(hidden)]`; the resource's public
  surface is the contracted one (the `WorldModel` trait), not its storage layout.
- **Documenting the type but not the contract.** Stating "a triple store" without
  saying its scope, that it is accessed via `Res<T>` only, how it mutates, and
  which plugin registers it. The contract — scope, access, mutation, provider,
  and the axis it sits on — is the part a system author actually needs.
