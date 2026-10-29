# API Documentation Standard

An `API` is a typed handle a plugin publishes (via `insert_api`) so other
plugins can contribute to or drive it at wiring time. `polaris_code` currently
publishes **no** APIs of its own — its plugins register tools and resources and
consume upstream APIs (`SessionsAPI`, `ToolRegistry`, the TUI `TuiRegistry`)
rather than exposing new ones. This page is the workspace's normative standard;
it adapts the canonical
[Polaris API standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/api.md#documentation-standard).

## Documentation Standard

If this crate ever publishes an `API`, its rustdoc MUST include the following.
Flag an ISSUE if any required section is missing.

| Section | Content |
|---------|---------|
| Purpose + consumer goal | What capability it exposes **and the consumer goal it serves**. |
| `# Provided By` | The plugin that calls `insert_api` for this type. |
| `# Surface` | Method table with a one-line purpose per `pub` method. |
| `# Lifecycle` | When each method may be called (`build()`, `ready()`, runtime) and the consequence of calling outside that window. |
| `# Composition` | One of **Open extension**, **Provider-scoped**, or **Single-replace**. |
| `# Example` | A code block showing *both* the provider's `insert_api` call and a consumer's `server.api::<T>()` access. |

See the canonical Polaris standard linked above for the composition-policy
definitions and worked examples.
