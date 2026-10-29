A catalog of every [`API`](polaris::system::api::API) shipped by
`polaris_code_core`. APIs are the typed handles plugins publish so other plugins
can contribute to or drive them at wiring time.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans `src/` for `impl API for X` and asserts
> each name appears below. Adding a new API without listing it here will fail
> CI.

_none_ — `polaris_code_core` publishes no APIs of its own. Its plugins register
tools and resources and consume upstream APIs (`SessionsAPI`, `ToolRegistry`,
the TUI `TuiRegistry`) rather than exposing new ones. If that changes, add the
API here and document it per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/reference/api.md#documentation-standard).

The embedded Python, JavaScript, TypeScript, and TSX indexers add no API: they
populate the existing Code `ABox` behind `CodeIndexer` and `CodeGraphTools`.
