# API Documentation Standard

An `API` is a typed handle a plugin publishes (via `insert_api`) so other
plugins can contribute to or drive it at wiring time. `polaris-tui` publishes two:
`TuiRegistry` (panels contribute to it) and `TuiHandle` (other plugins drive the
shell through it). This page is the normative standard for API docs in this
crate; it adapts the canonical
[Polaris API standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/api.md#documentation-standard).

## Documentation Standard

An `API`'s rustdoc MUST include the following sections.

| Section | Content |
|---------|---------|
| Purpose + consumer goal | What capability it exposes **and the consumer goal it serves** (e.g. "contribute a panel to the shell"). |
| `# Provided By` | The plugin that calls `insert_api` for this type (`TuiPlugin` for both). |
| `# Surface` | Method table with a one-line purpose per `pub` method. |
| `# Lifecycle` | When each method may be called (`build()`, `ready()`, runtime) and the consequence of calling outside that window — e.g. `add_panel` after the registry is drained warns and no-ops. |
| `# Composition` | One of **Open extension** (`TuiRegistry` — any number of panels), **Provider-scoped**, or **Single-replace**. |
| `# Example` | A code block showing *both* the provider side and a consumer's `server.api::<T>()` access. |

See the canonical Polaris standard linked above for the composition-policy
definitions and worked examples.
