A catalog of every [`API`](polaris::system::api::API) shipped by
`polaris-ontrace`. APIs are the typed handles plugins publish so other plugins
can contribute to or drive them at wiring time.

Each entry links to the API's rustdoc page, which documents its surface,
lifecycle, composition policy, and a provider+consumer example per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_ontrace/docs/reference/api.md#documentation-standard).

# Ingest

The two halves of the fan-in dispatcher provided by
[`TraceIngestPlugin`](crate::TraceIngestPlugin): many sources push into one,
one backend claims the other.

| API | Composition | Description |
|-----|-------------|-------------|
| [`TraceIngestAPI`](crate::TraceIngestAPI) | Open extension | A cloneable [`mpsc::Sender`](tokio::sync::mpsc) handle. Any number of source plugins push [`IOMessage`](polaris::plugins::IOMessage)s into the shared channel via `push`/`sender`. |
| [`TraceIngestRx`](crate::plugins::trace_ingest::TraceIngestRx) | Provider-scoped (single-claimant) | Holds the channel's receiver. `take()` hands it to exactly one backend; a second call panics — the type-level enforcement of "one backend per server". |

# Inspect

| API | Composition | Description |
|-----|-------------|-------------|
| [`InspectAPI`](crate::InspectAPI) *(feature `oxigraph`)* | Provider-scoped | `ask(question)` runs an ephemeral session against the graph and returns the agent's answer. Provided by [`InspectPlugin`](crate::InspectPlugin); consumed by the `ontrace inspect` CLI and TUI chat panel. |
