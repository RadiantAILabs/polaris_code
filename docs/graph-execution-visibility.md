# Watching a graph that is not the agent loop

Proof: `crates/polaris_code_core/tests/graph_execution_visibility.rs`.
Code: `crates/polaris_code_core/src/progress.rs`,
`crates/polaris_code_core/src/cosmos/host.rs`.

`polaris_code` runs graphs other than the agent's `ReAct` loop — the six coding
topologies in `cosmos/graphs.rs`, and whatever a topology pack adds later. The
progress bridge that makes work visible was written against the agent loop, so
every other graph used to execute in complete silence. A long step nobody can
see is indistinguishable from a hang.

## Why a topology run was silent

Three independent causes, all local to this crate, are pinned by tests.

1. **The middleware never fired.** `cosmos::host` builds its own
   `GraphExecutor` and called `execute(graph, ctx, None, None)`. A middleware
   only applies to an execution that was handed the `MiddlewareAPI` it was
   registered on; `None` is the default empty chain.
2. **No `UserIO` resolved.** The host's run context is built from scratch
   (`SystemContext::new()`) and held only `CodingWorkspace` and
   `SweepWorklist`. Even had the middleware fired, its `UserIO` read would have
   missed and it would have degraded to silence, which is what it is supposed
   to do.
3. **The naming function knew six names.** `activity_for` matched the agent
   loop's nodes and returned `None` for everything else.

Generalising (3) alone changed nothing observable — the first version of
`a_topology_graph_reports_per_node_status` still saw an empty list. That is the
evidence that (3) was not the cause on its own. `with_no_user_io_...` and
`instrumentation_changes_nothing_...` each hold one of (1) and (2) back and
show the run goes quiet again, so each is individually necessary.

No framework change was needed. The renderer already buckets any status message by its `node`
metadata, whatever the node is called.

## The rule by which a node gets a description

`NodeActivity` is a registry, not a match arm:

1. a node with a registered description uses it — a working label, an explicit
   *idle* (which clears the line and resets the sequence), or explicit
   *silence*;
2. every other node is described by its own identifier, humanised:
   `predict_edit` → `Predict edit…`.

**The default is visible; invisibility is a decision someone recorded.**

The alternative default — say nothing about a name you do not recognise — is
exactly what produced the silence above, and it would produce it again for the
seventh graph and the eighth. It also fails the only question a user asks of a
running graph: is it moving, and where is it. A developer identifier answers
that; nothing does not.

A raw identifier is not prose, and that is an accepted cost. It is bounded: the
panel's Ctrl-O node timeline already prints the raw node key beside the line,
so the humanised form introduces no name the user was not already being shown —
it is the same name, easier to read. An author who wants better prose writes it
once; an author who wants silence says so once. Neither requires editing
`progress.rs`.

The agent loop's six labels are registered exactly as they read today
(`NodeActivity::with_agent_loop`), including `receive_user_input` as *idle* and
the two structural systems as *silent*. This is a superset, not a replacement.

## How a pack describes its own nodes

`NodeActivity` is a build-phase resource, so a plugin contributing graphs
registers against it in `build`:

```rust
#[plugin(id = "example::pack", version = "0.1.0")]
impl Plugin for ExamplePackPlugin {
    fn build(&self, activities: Extends<NodeActivity>) {
        activities.describe("predict_edit", "Predicting the edit…");
        activities.silence("act_edit");
    }
}
```

Both halves are proved by `a_pack_describes_and_silences_its_own_nodes`.

## How the host reaches the session's watcher

Two absent-tolerant reads in `cosmos::host::select_and_run`, both before the
readmission loop:

- `attach_user_io` takes the `UserIO` **factory** the session retained on its
  resource entry and mints a fresh instance into the pristine run context.
  `UserIO` is not cloneable, but a fresh one over the same provider reaches the
  same panel. Adding a resource cannot change an admission verdict: live
  validation reports *missing* resources, never extra ones.
- `read_instrumentation` clones `GraphInstrumentation` — a global resource in
  which `ProgressMiddlewarePlugin` publishes the app's `MiddlewareAPI` — and
  passes it to `execute`. This indirection exists because `SessionsAPI` keeps
  its middleware without exposing a getter, and APIs are not visible in a
  `SystemContext`.

If either is absent — headless, a test, an embedding with no front end, no
progress plugin installed — the host runs the graph exactly as it did before.
Nothing inserted, nothing logged, no behavioural difference.

`GraphInstrumentation` is host wiring rather than a consumer-facing resource,
so it is listed in `INTERNAL_RESOURCES` in `tests/resource_catalog.rs`.

## What is still true of a topology turn

Reporting progress changed none of it. A selected topology still runs in the
host's own context, calls no model, does not advance the session's turn number,
does not write the transcript and does not move the `TokenMeter` — see
`docs/topology-selection.md`. `a_topology_turn_calls_no_model_and_leaves_the_session_untouched`
asserts all five with the instrumentation attached, counting provider calls
directly.

## Verified

`crates/polaris_code_core/tests/graph_execution_visibility.rs`, five tests over
a real temporary crate and a real `Server`:

1. **`a_topology_graph_reports_per_node_status`** — the
   `code.prediction-error-edit` graph emits seven status messages, in node
   order, each carrying the `node` key the panel buckets on and `seq` 1–7.
2. **`with_no_user_io_the_same_graph_runs_identically_and_says_nothing`** — the
   same turn without an IO plugin produces no status, no error, and the same
   registration, node order and file contents.
3. **`instrumentation_changes_nothing_about_the_run`** — the same turn with and
   without `ProgressMiddlewarePlugin` gives the same registration, the same
   node count, the same summary, the same verifier observations in the same
   order and the same file on disk. Node *order* is pinned by test 1.
4. **`a_pack_describes_and_silences_its_own_nodes`** — a later plugin renames
   one node's label and suppresses another; the suppressed node still executes.
5. **`a_topology_turn_calls_no_model_and_leaves_the_session_untouched`** — zero
   provider calls, turn number unmoved, transcript empty, `TokenMeter` at zero.
