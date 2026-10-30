A catalog of every [`API`](polaris::system::api::API) shipped by `polaris-tui`.
APIs are the typed handles plugins publish so other plugins can contribute to or
drive them at wiring time.

Each entry links to the API's rustdoc page, which documents its surface,
lifecycle, composition policy, and a provider+consumer example per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris-tui/docs/reference/api.md#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans `src/` for `impl API for X` and asserts
> each name appears below. Adding a new API without listing it here will fail
> CI.

# Shell

Both APIs are provided by [`TuiPlugin`](crate::TuiPlugin).

| API | What it enables |
|-----|-----------------|
| [`TuiRegistry`](crate::TuiRegistry) | The cross-plugin panel contribution surface. Panels call `add_panel` in their `build()`; `TuiPlugin` drains it once in `ready()`. **Open extension** — any number of panels. |
| [`TuiHandle`](crate::TuiHandle) | Runtime handle to the running shell. `run_in_terminal()` serializes interactive child handoffs and restores terminal modes; `shutdown()` requests cooperative exit; `wait()` joins once. Panels receive a clone before initialization. |

Approval rendering uses the ordinary typed [`ApprovalChoice`](crate::ApprovalChoice)
protocol, not another framework API. Hosts encode choices using
[`APPROVAL_CHOICES`](crate::APPROVAL_CHOICES); replies identify the selected scope
with [`APPROVAL_CHOICE`](crate::APPROVAL_CHOICE). Prompt IDs correlate cancellation
and responses so a resolved approval cannot answer a later receive.
