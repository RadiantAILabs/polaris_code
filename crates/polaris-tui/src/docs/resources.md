A catalog of every consumer-facing [`GlobalResource`](polaris::system::resource::GlobalResource)
and [`LocalResource`](polaris::system::resource::LocalResource) shipped by
`polaris-tui` — the types a `#[system]` consumes via `Res<T>`.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/resource_catalog.rs`) that scans `src/` for `impl GlobalResource for
> X` / `impl LocalResource for X` and asserts each name appears below — unless
> it is listed in that test's `INTERNAL_RESOURCES` allowlist. Adding a new
> consumer-facing resource without listing it here will fail CI.

The shell communicates through its [`TuiRegistry`](crate::TuiRegistry) and
[`TuiHandle`](crate::TuiHandle) APIs and owns its terminal state inside a
background task, not in the resource registry. The one resource it ships is the
feed a panel plugin shares with whatever produces the panel's data. Document any
other per the
[resource documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris-tui/docs/reference/resources.md#documentation-standard).

# Panels *(feature `panels`)*

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`GraphFeed`](crate::panels::GraphFeed) | Global | The graph the Graph tab draws and what is running in it. A producer `publish`es the graph it normally runs, `announce`s a different one before running it, opens a run with `begin_run` and reports each node with `enter`; guards report completion when dropped, even if the turn is cancelled. The tab reads a `snapshot` only when `version` moved, so a producer's cost is one lock and one atomic increment. Provided by [`GraphPanelPlugin`](crate::panels::GraphPanelPlugin); a headless host installs none, so a producer must tolerate its absence. Global because a shell shows one graph. |
