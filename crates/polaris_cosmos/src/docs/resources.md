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

## Durable flow

All thirteen are global and consumer-supplied: the host builds each one and
registers it as a global, directly or through
[`FlowPlugin`](crate::flow::FlowPlugin), which also settles spend left in flight
by an earlier run. Systems read them with `Res<T>`. The durable books change
through `&self` under an internal lock and save before memory takes the change;
`RunHandle`, `RoleBook`, `FlowSettings` and `CallDeadline` are immutable. Seven
of them persist files under the run root: `SpendBook`, `StepJournal`,
`RepairCounters`, `LeaseBook`, `Worklist`, `PhaseControl` and `EventLog`.

| Resource | Purpose | Built with |
|---|---|---|
| [`RunHandle`](crate::flow::RunHandle) | Run id, run root and whether the run resumes an earlier one. | Struct literal |
| [`RoleBook`](crate::flow::RoleBook) | Role prompts, limits, spend ceilings and tool grants by name. | `RoleBook::load` |
| [`AgentRunner`](crate::flow::AgentRunner) | Replaceable executor for one role call; the scripted runner is the offline point. | `AgentRunner::new` |
| [`StopFlag`](crate::flow::StopFlag) | Cooperative stop broadcast for runners and systems; no flow block reads it. | `StopFlag::default` |
| [`SpendBook`](crate::flow::SpendBook) | Durable integer reservations and settlements, with a cost ledger. | `SpendBook::open` |
| [`StepJournal`](crate::flow::StepJournal) | Durable step outputs and numbered workspaces; restores finished steps. | `StepJournal::open` |
| [`RepairCounters`](crate::flow::RepairCounters) | Shared named repair caps that survive a restart. | `RepairCounters::open` |
| [`LeaseBook`](crate::flow::LeaseBook) | Shared cap of numbered work units across parallel lanes. | `LeaseBook::open` |
| [`FlowSettings`](crate::flow::FlowSettings) | Asset root, task directory, model identity and the output-size cap. | Struct literal |
| [`PhaseControl`](crate::flow::PhaseControl) | Phase enablement, deadlines, typed statuses and the halt state. | `PhaseControl::open` |
| [`Worklist`](crate::flow::Worklist) | Runtime-selected sequential items and persisted outcomes. | `Worklist::open` |
| [`EventLog`](crate::flow::EventLog) | Whole-line, flushed JSON-lines events in their recorded order. | `EventLog::open` |
| [`CallDeadline`](crate::flow::CallDeadline) | Optional wall-clock limit for admitting new calls. | `CallDeadline::new` |
