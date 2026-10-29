# API Documentation Standard

An **API** in `polaris_ontrace` is a handle a plugin publishes with
`server.insert_api(T)` and that other code fetches with `server.api::<T>()`. It
is how one plugin hands a typed capability to the rest of the server — a sender
to push into, a receiver to claim, a pre-bound `ask()` helper. Because APIs are
the wiring seams between plugins, their docs must state who provides them, what
methods exist, when those methods are legal to call, and how multiple consumers
compose. This page is the normative standard for API docs in this crate.

## Documentation Standard

An API type's rustdoc MUST open with a **purpose + when-to-use** sentence ("push
inbound messages into the extraction channel — used by every source plugin").
Past that, the following sections apply.

### Required sections

Flag an ISSUE if any of these is missing.

| Section | Content |
|---------|---------|
| Purpose + when-to-use | What the API is for and which side of the wiring uses it. |
| `# Provided by` | The plugin whose `build()`/`ready()` calls `insert_api`, or "consumer-supplied" if the consumer inserts it. |
| `# Surface` | Table `Method \| Description`, one row per `pub` method. |
| `# Lifecycle` | When each method may be called — `build()` / `ready()` / runtime — and the consequence of calling outside that window. |
| `# Composition` | Exactly one of **Open extension**, **Provider-scoped**, or **Single-replace** (see Composition Policy below). |
| `# Example consumers` | One to three in-repo consumers and what each contributes. |
| `# Example` | A code block showing **both** the provider's `insert_api` and a consumer's `server.api::<T>()`. |

## Composition Policy

Every API doc MUST classify itself as one of:

- **Open extension** — any plugin may contribute through it; the API guards a
  shared collection (typically behind an mpsc sender or `RwLock`). Multiple
  registrants coexist. *Ontrace example:* `TraceIngestAPI` — every source plugin
  fetches it and pushes; the dispatcher fans them in over one channel.
- **Provider-scoped** — only the providing plugin mutates the underlying state;
  the public methods are read-only or single-shot from a consumer's view.
  *Ontrace examples:* `InspectAPI` (all Q&A flows through the plugin's agent;
  consumers only call `ask`), `TraceIngestRx` (a single backend claims the
  receiver via `take()` exactly once).
- **Single-replace** — a later `insert_api` of the same type replaces the prior
  one wholesale. Use when there is meant to be exactly one live instance and
  re-registration is intentional override.

## Canonical Exemplars

- **`TraceIngestAPI`** (`crate::TraceIngestAPI`, `src/plugins/trace_ingest.rs`)
  — **Open extension**. Holds a clone of the ingest mpsc sender; `push` and
  `sender` are the surface. Every source (`StdinIOPlugin`, `FileTailIOPlugin`,
  `OtlpIOPlugin`, `ClaudeCodeIOPlugin`) fetches it in `ready()` and pushes
  `IOMessage`s. The exemplar for "many independent contributors, one channel."
- **`TraceIngestRx`** (`crate::plugins::trace_ingest::TraceIngestRx`) —
  **Provider-scoped / single-claimant**. Surface is one method, `take()`, which
  hands the receiver to exactly one extraction backend; a second `take()`
  panics. The exemplar for documenting a one-shot handoff and its
  `# Lifecycle` ("call once in the backend's `ready()`").
- **`InspectAPI`** (`crate::plugins::inspect::InspectAPI`, feature `oxigraph`) —
  **Provider-scoped**. Surface is `ask(question) -> answer`; every call spawns a
  fresh ephemeral session through the plugin's `InspectAgent`. The exemplar for
  a pre-bound, read-only-from-the-caller helper where all state stays inside the
  providing plugin.

## Anti-Patterns

- **Unstated composition.** An API doc that omits `# Composition` leaves
  consumers guessing whether a second registrant is an error or an extension.
- **Undocumented call window.** Publishing methods without a `# Lifecycle` —
  e.g. not saying `TraceIngestRx::take` is `ready()`-only and single-shot, or
  that `server.api::<T>()` returns `None` before the provider's `build()` runs.
- **Surface drift.** A `# Surface` table that omits a `pub` method, or describes
  a method's effect without its failure mode (`TraceIngestAPI::push` returns the
  message back if the worker has shut down).
- **Provider/consumer split missing from `# Example`.** Showing only the
  consumer call (`server.api::<T>()`) without the provider's `insert_api`, or
  vice versa, hides half the contract.
