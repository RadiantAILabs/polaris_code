# Plugin Documentation Standard

`polaris_cosmos` uses `polaris::Plugin`s for cognitive or world-model surfaces
that need server lifecycle integration: memory backends, policy registries,
observers, and host bridges. Every plugin's rustdoc is a contract: it tells a
downstream consumer what the plugin puts on the server, what it expects to
already be there, and how to register it. This page is the normative standard
for plugin documentation in this crate.

## Documentation Standard

A plugin's module- or struct-level rustdoc MUST open with a one- or two-sentence
**summary that states what the plugin does AND when to reach for it** (e.g.
"registers a query-only in-memory `WorldModel` — the trivial-default `W` backend;
reach for it before you need prediction or learned parameters"). Past the
summary, the following sections apply.

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
| `# Axis` | The plugin fills an axis slot of the `M = (G, W)` contract. Name the axis (memory backend, store layout, recall, consolidate, rewire, …), the point on it this plugin occupies, and whether it is the trivial default. |
| `# Tools Provided` | It registers `#[tool]` / `toolset` with a `ToolRegistry`. |
| `# Hooks Registered` | It registers session/server hooks. |
| `# Lifecycle` | It has a non-trivial `ready()`/`cleanup()`, spawns a background task, claims a one-shot resource, or is feature-gated. Bullet list of what happens and when. |
| `# Extends` | It contributes to another plugin's API or registry (e.g. a channel installer that registers against a backend) rather than owning its own. |

## Canonical Exemplars

[`TopologyCatalogPlugin`](../../src/topology/plugin.rs) is the in-repo exemplar
for a capability provider with a two-phase registry lifecycle: mutable resource
during `build()`, admission-completeness audit followed by an immutable global
during `ready()`. Its topology-pack test demonstrates two independent
`Extends<TopologyCatalog>` contributors producing the same sealed contents in
either insertion order. For an
API provider or a capability consumer, follow the canonical exemplars in
`polaris_ontrace` (`crates/polaris_ontrace/docs/reference/plugins.md`) —
`TraceIngestPlugin` and `StdinIOPlugin`, respectively.

## Anti-Patterns

- **Insertion-order coupling.** Documenting "register this after the backend"
  instead of declaring the dependency in `access()`. The resolver binds
  `requires`/`provides` by type and errors at `finish()`; prose ordering is not a
  contract. A channel installer says it needs a `WorldModel` capability, not
  "register me after the backend plugin."
- **Circular dependencies.** A plugin requiring a capability it also provides, or
  two plugins each requiring the other. The `access()` resolver cannot order
  these.
- **Camping in a default.** A `W`-backend or channel plugin whose doc presents
  its one behavior as the only behavior, with no `# Axis` note locating it on its
  axis. Every substrate part is a *point*; the doc must say which, so the axis
  reads as open.
- **Happy-path-only docs.** Omitting the failure and no-op modes — what `build()`
  does when a required provider is absent (a typed `ServerBuildError` at
  `finish()`, not a runtime panic), and what a feature-gated plugin compiles to
  without its feature.
