# Known issues

This file records current technical limitations that are useful to contributors.
Remove an entry when its behavior changes, and keep the public behavior docs in
sync with any fix.

## Agent and context

- **Recovery bypasses normal context projection.** The recovery system in
  `agents/code_agent.rs` sends the canonical message list instead of applying
  the active retrieval window. A retry after provider context overflow can
  therefore be larger than the request that failed. Recovery should use the
  same `RetrievalPolicy` projection as the normal action path.

- **Agent setup assumes a multithreaded Tokio runtime.** Project-context
  discovery currently uses `block_in_place`, which panics on a current-thread
  runtime. Move discovery to an asynchronous lifecycle hook or document the
  runtime requirement on `CodeAgent`.

- **Token-delta retry accounting needs focused tests.** The conversion from
  cumulative to incremental usage and the rule that committed output cannot be
  retried should be extracted into a pure helper and tested for zero,
  decreasing, and repeated values.

- **Git context is captured once per session.** Project instructions refresh at
  turn boundaries, but branch, status, and recent-commit context are captured
  during setup. Refreshing those values would invalidate the cache-stable prompt
  prefix, so any change needs an explicit cache policy.

## Tools and persistence

- **Reified tool-call arguments are not secret-redacted.** Tool output is
  redacted before entering the transcript and session graph, but serialized
  arguments in `agents/trajectory.rs` are persisted as supplied. Inline
  credentials in a command can therefore remain in local session state. Apply
  the same redaction seam before recording arguments.

- **An interrupted atomic edit can leave a scratch file.** File edits and
  rewind writes use a same-directory temporary file so rename remains atomic.
  Normal errors clean it up, but process termination between write and rename
  can leave a `.polaris-edit-*` or `.polaris-rewind-*` file in the tree.

- **Concept enrichment is rebuilt after each indexed mutation.** The current
  implementation clears and derives the complete role graph after an edit.
  Incremental derivation or debouncing would reduce work on large repositories.

- **New-file integration is incomplete.** `create_file` uses the same
  permission grant and journal as `edit_file`, but its confirmation view is a
  generic argument display, newly created source files are not immediately
  re-indexed, and rewinding a creation restores an empty file instead of
  removing it.

- **Tool argument detail omits a dedicated `glob` projection.** The transcript
  has a concise `search` summary, while `glob` falls back to rendering its full
  argument object.

## Code-index freshness

The freshness check in `tools/code_graph.rs` intentionally trades exhaustive
hashing for bounded filesystem work:

- staleness is detected by modification time and size, so a same-size rewrite
  inside a coarse filesystem timestamp interval can be missed;
- a file that fails to parse has no indexed stamp and is retried on each
  reconciliation;
- a negative query stats the whole eligible tree, while a resolving query does
  not walk it.

`edit_file` does not depend on this heuristic because it directly re-indexes
the path it changed.

## Topology host

- **Two adjacent topology-only tasks can reuse evidence identities.** Evidence
  IDs derive from session ID, turn number, and event label, while a topology
  task does not advance the conversational turn number. A per-session topology
  ordinal would preserve deterministic replay without colliding.

- **Directed outcomes lack provenance in aggregate evidence.** The immediate
  turn record says whether a registration was requested, but Cosmos outcome
  observations do not. A directed run can therefore influence later autonomous
  rankings as if it had been selected autonomously. Topology comparison avoids
  this by using a fresh evidence store per arm; ordinary directed submissions
  still share the session store.

- **Live-admission refusal does not re-enter gate-plan revision.** The planner
  revises materialization refusals before live admission. If the materialized
  graph is refused by live admission, the host records the refusal and moves to
  the next candidate instead of asking for another plan for the same candidate.

- **Verifier capability is inferred too broadly.** `candidate_facts` currently
  marks every candidate deterministic, which is true for the coding graphs but
  not for the published default conversational loop. That loop is excluded
  before scoring today; future hosts that rank it should source the capability
  from the registration.

## Session durability

- A completed interactive turn is persisted between turns. A process that dies
  during a turn loses that incomplete turn because the session context is held
  for the duration of execution.
- A rewind performed after the last completed turn is not persisted until the
  next save point. The file restoration has already happened, but an abrupt
  stop can reopen the unrewound conversation history.
- Cooperative headless SIGINT and SIGTERM both currently report exit code 130;
  only the immediate SIGTERM escape reports 143.

## Test and API debt

- `CodeAgentPlugin` should declare its framework dependencies explicitly.
- `AgentLlm`, `PreparedContext`, and `TokenMeter` should implement `Debug`.
- Several `edit_file` error branches and progress-sequence behavior need direct
  tests.
- The `TokenMeter` concurrency claim has only sequential coverage.
- `Decision::new` and `Decision::with_policy` are public but unused, and
  `Decision.kind` would be safer as an enum.
- `oxigraph` is pre-1.0 and appears in public signatures, so consumers must use
  a compatible version.

See [`secret-redaction.md`](secret-redaction.md),
[`parallel-tool-batches.md`](parallel-tool-batches.md), and
[`default-loop-publication.md`](default-loop-publication.md) for the contracts
related to several of these limitations.
