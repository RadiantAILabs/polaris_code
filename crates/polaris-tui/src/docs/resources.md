A catalog of every consumer-facing [`GlobalResource`](polaris::system::resource::GlobalResource)
and [`LocalResource`](polaris::system::resource::LocalResource) shipped by
`polaris-tui` — the types a `#[system]` consumes via `Res<T>`.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/resource_catalog.rs`) that scans `src/` for `impl GlobalResource for
> X` / `impl LocalResource for X` and asserts each name appears below — unless
> it is listed in that test's `INTERNAL_RESOURCES` allowlist. Adding a new
> consumer-facing resource without listing it here will fail CI.

_none_ — `polaris-tui` registers no consumer-facing resources. The shell
communicates through its [`TuiRegistry`](crate::TuiRegistry) and
[`TuiHandle`](crate::TuiHandle) APIs and owns its terminal state inside a
background task, not in the resource registry. If that changes, add the resource
here and document it per the
[resource documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris-tui/docs/reference/resources.md#documentation-standard).
