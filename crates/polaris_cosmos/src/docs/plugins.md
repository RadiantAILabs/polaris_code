A catalog of every [`Plugin`](polaris::system::plugin::Plugin) shipped by
`polaris_cosmos`, grouped by the part of the world-model substrate it provides.

Each entry links to the plugin's rustdoc page, which is the canonical source for
its provided resources, APIs, dependencies, lifecycle, and a registration
example per the [plugin documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/plugins.md#documentation-standard).
See the [integration guide](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/guide.md)
to go the other way — from a downstream goal to the plugins that satisfy it.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/plugin_catalog.rs`) that scans `src/` for `impl Plugin for X` and
> asserts each name appears below. Adding a new plugin without listing it here
> will fail CI.

## Topology discovery

- [`TopologyCatalogPlugin`](crate::topology::TopologyCatalogPlugin) — provides
  the replaceable [`TopologyCatalog`](crate::topology::TopologyCatalog)
  capability. Topology-pack plugins extend the catalog during `build()`; the
  provider audits complete admission metadata, seals, and globalizes it during
  `ready()` for immutable runtime discovery. Missing type explanations or
  partial registrations fail assembly before runtime preflight.

## Topology evidence

- [`TopologyEvidencePlugin`](crate::evidence::TopologyEvidencePlugin) — provides
  the replaceable [`TopologyEvidence`](crate::evidence::TopologyEvidence)
  capability using the bounded, process-local in-memory provider. Hosts and
  recorders append typed events explicitly; the plugin starts no task and
  performs no outcome interpretation.
