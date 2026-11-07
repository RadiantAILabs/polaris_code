A catalog of every [`API`](polaris::system::api::API) shipped by
`polaris_cosmos`. APIs are the typed handles plugins publish so other plugins
can contribute to or drive them at wiring time.

Each entry links to the API's rustdoc page, which documents its surface,
lifecycle, composition policy, and a provider+consumer example per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/api.md#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans `src/` for `impl API for X` and asserts
> each name appears below. Adding a new API without listing it here will fail
> CI.

> This crate currently defines no APIs.

The [`flow`](crate::flow) module's graph builders are ordinary functions that
return a `Graph`, not APIs. The
[integration guide](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/guide.md#durable-agent-workflows)
lists them with the resources each one reads.
