# API Documentation Standard

An **API** in `polaris_cosmos` is a handle a plugin publishes with
`server.insert_api(T)` and that other code fetches with `server.api::<T>()`. It
is how one part of the substrate hands a typed capability to the rest of the
server — a handle to drive a rewire bridge, a hook to register a channel
policy. Because APIs are the wiring seams between plugins, their docs must state
who provides them, what methods exist, when those methods are legal to call, and
how multiple consumers compose. This page is the normative standard for API
documentation in this crate.

## Documentation Standard

An API type's rustdoc MUST open with a **purpose + when-to-use** sentence ("apply
a rewire-proposed schema change to the host graph — used by the structure-proposal
channel once a proposal passes the gate"). Past that, the following sections
apply.

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
  registrants coexist.
- **Provider-scoped** — only the providing plugin mutates the underlying state;
  the public methods are read-only or single-shot from a consumer's view.
- **Single-replace** — a later `insert_api` of the same type replaces the prior
  one wholesale. Use when there is meant to be exactly one live instance and
  re-registration is intentional override.

## Canonical Exemplars

> `polaris_cosmos` ships no APIs yet. Until its first API lands, follow the
> canonical API exemplars in
> `polaris_ontrace` (`crates/polaris_ontrace/docs/reference/api.md`) — `TraceIngestAPI` for
> Open extension (many pushers, one channel), `TraceIngestRx` for a
> Provider-scoped single-shot handoff, `InspectAPI` for a pre-bound read-only
> helper. This section is updated with cosmos's own exemplars as they arrive.

## Anti-Patterns

- **Unstated composition.** An API doc that omits `# Composition` leaves
  consumers guessing whether a second registrant is an error or an extension.
- **Undocumented call window.** Publishing methods without a `# Lifecycle` — e.g.
  not saying a handoff handle is `ready()`-only and single-shot, or that
  `server.api::<T>()` returns `None` before the provider's `build()` runs.
- **Surface drift.** A `# Surface` table that omits a `pub` method, or describes
  a method's effect without its failure mode.
- **Provider/consumer split missing from `# Example`.** Showing only the consumer
  call (`server.api::<T>()`) without the provider's `insert_api`, or vice versa,
  hides half the contract.
