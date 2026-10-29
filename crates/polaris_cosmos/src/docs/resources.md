# Resources

A catalog of every consumer-facing [`GlobalResource`](polaris::system::resource::GlobalResource)
and [`LocalResource`](polaris::system::resource::LocalResource) shipped by
`polaris_cosmos` — the types a `#[system]` consumes via `Res<T>`.

Each entry links to the resource's rustdoc page, which documents its scope,
provider, access pattern, and a real-shape consuming system per the
[resource documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/resources.md#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/resource_catalog.rs`) that scans `src/` for `impl GlobalResource for
> X` / `impl LocalResource for X` and asserts each name appears below — unless
> it is listed in that test's `INTERNAL_RESOURCES` allowlist (per-session
> plumbing not part of the consumer surface). Adding a new consumer-facing
> resource without listing it here will fail CI.

## Topology discovery

- [`TopologyCatalog`](crate::topology::TopologyCatalog) — global,
  startup-populated and sealed before execution. Systems read it through
  `Res<TopologyCatalog>` to capture an immutable discovery snapshot; the
  catalog atomically retains every owned graph with its selection reference,
  requirements, stable signature explanation, configuration schema identity,
  and attributed author priors. Its sealed snapshot exposes deterministic
  candidate order and stable content identity.
  [`TopologyCatalogPlugin`](crate::topology::TopologyCatalogPlugin) provides its
  default lifecycle and rejects incomplete registrations before globalization,
  while topology packs contribute complete registrations through
  `Extends<TopologyCatalog>`.

## Topology evidence

- [`TopologyEvidence`](crate::evidence::TopologyEvidence) — global, cloneable
  facade over a replaceable append-only provider. Systems use
  `Res<TopologyEvidence>` to record or read selection episodes and capture
  immutable statistics. [`TopologyEvidencePlugin`](crate::evidence::TopologyEvidencePlugin)
  supplies the bounded in-memory default; another plugin may provide the same
  versioned contract around a durable backend.
