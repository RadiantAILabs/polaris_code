# Resource Documentation Standard

A resource is typed state a `#[system]` consumes via `Res<T>` / `ResMut<T>`.
`polaris-tui` currently registers **no** consumer-facing resources — the shell
communicates through its `TuiRegistry` / `TuiHandle` APIs and owns its terminal
state inside a background task, not in the resource registry. This page is the
normative standard for resource docs in this crate should that change; it
adapts the canonical
[Polaris resource standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/resources.md#documentation-standard).

## Documentation Standard

If this crate ever registers a consumer-facing resource, its rustdoc MUST
include the following sections.

| Section | Content |
|---------|---------|
| Purpose + when-to-use | What capability the resource exposes **from a system author's perspective**. |
| `# Scope` | `Global` or `Local`, **and one sentence of justification**. |
| `# Provided By` | The plugin that registers it (with feature gate if any), or "consumer-supplied". |
| `# Access` | Valid parameter types (`Res<T>` only, or `Res<T>` + `ResMut<T>`) and the mutation contract; for globals, any interior mutability. |
| `# Alternatives` | Related/variant resources, or a `_none_` row. |
| `# Example` | A real-shape `#[system]` consuming the resource. |

Internal, per-panel state that no downstream system reads is exempt — keep it
`#[doc(hidden)]` or non-`pub`, and list it in the `INTERNAL_RESOURCES` allowlist
in `tests/resource_catalog.rs` if it ever gains a `Resource` impl.
