# The Graph tab

Proof: `crates/polaris_code/src/graph_view.rs` (unit tests),
`crates/polaris_code/tests/graph_execution_visibility.rs`,
`crates/polaris_code/tests/graph_tab_default_loop.rs`,
`crates/polaris_code/tests/shell_tabs.rs`,
`crates/polaris-tui/src/panels/graph/`.
Code: `crates/polaris-tui/src/panels/graph/`,
`crates/polaris_code/src/graph_view.rs`, `crates/polaris_code/src/tabs.rs`.

The default `polaris` shell has three tabs: **Conversation**, **Graph** and
**Help**. The Graph tab draws the graph that is running and lights up the node
that is executing, live.

```text
 CodeAgent · 25 nodes · run 3 · running                        1-23/29 · following
 ✓ receive_user_input                                                       0.0s
 ✓ refresh_project_docs                                                     0.0s
 ✓ init_loop                                                                0.0s
 ▸ ↻ agent_loop  repeats                                           running 4.2s
   ├─ ✓ • apply_steering                                                    0.0s
   ├─ ✓ • compact_context                                                   0.0s
   └─ ? manual_compaction                                           running 4.2s
      ├─ yes → • finish_compaction
      └─ no
         ├─ ✓ • prepare_context                                             0.1s
         ├─ ▶ • act                                                 running 2.1s
```

- `▶` is the node running now, with how long it has run. `▸` is a node that is
  running something inside it (a loop, a choice). `✓` ran in this run, with the
  time of its last pass and `×N` if it ran more than once.
- A sequence reads top to bottom. A choice, switch, parallel group, loop or
  scope draws its branches nested under it; a branch that is one step shares its
  row with its label. An error handler is drawn as `on error` under the loop it
  guards, or after the main chain.
- A graph taller than the terminal scrolls (`↑` `↓`, `PgUp` `PgDn`, `Home`
  `End`, mouse wheel). The tab follows the running node until you scroll; `f`
  follows it again.

## Which graph it shows

Whichever one is running. Until a turn runs it shows the agent's own graph
(`CodeAgent`). When `cosmos::host` selects a topology for a turn it announces
that topology's graph just before it executes, and the tab draws it, named by
its registration id. The next turn that does not announce one shows the agent's
graph again. A topology that was just run stays on screen, with what ran in it
marked, until the next run starts.

## How it is built

`polaris-tui` is a pure renderer with no dependency on the graph engine.

| Piece | Where | Job |
|-------|-------|-----|
| `GraphModel` | `polaris-tui` | The shape to draw: steps, branches, node kinds. No engine types. |
| `GraphFeed` | `polaris-tui` | Shared handle: the model and what is running. A global resource. |
| `GraphPanel` / `GraphPanelPlugin` | `polaris-tui` | The tab. Reads the feed when it changed. |
| `graph_model` | `polaris_code::graph_view` | Converts a framework `Graph` into a `GraphModel`. |
| `GraphViewPlugin` | `polaris_code::graph_view` | Publishes the agent's graph as the default. |
| `observe` | `polaris_code::graph_view` | Middleware that reports each node's start and finish. |
| `announce` | `polaris_code::graph_view` | Used by `cosmos::host` to name the graph it is about to run. |

The activity comes from the same framework middleware seam as the status line:
`ProgressMiddlewarePlugin` registers `observe` on the chain it publishes. Every
node kind is reported, because the framework wraps a decision, loop or scope
around the nodes it runs. Nothing in the agent or in any graph changed.

### Why a shared handle and not a `UserIO` message

The conversation panel owns the host's `UserIO` and consumes every message sent
through it. A second panel cannot receive from it unless the conversation panel
forwards, which would put the Graph tab's data model inside the conversation
panel. A cloneable handle over shared state is the shape `LogBuffer` already has
for the log subscriber and two panels, and it fits this data:

- every write overwrites state, so the panel only ever draws the latest, however
  many writes landed between frames (coalescing needs no queue);
- a write is one short lock and one atomic increment; the panel polls the
  version once per tick and reads a snapshot only when it moved;
- the layout is rebuilt only when the model is replaced, not when activity
  changes.

Measured in a debug build: an enter/leave pair costs about 0.5 µs; a tick plus a
full 80×24 render of the 25-node agent graph about 0.8 ms. The tests
`many_writes_between_frames_cost_one_read_and_no_layout` and
`a_change_of_activity_does_not_rebuild_the_layout` pin the counts.

### Matching a running node to a drawn one

Middleware names a node by its framework id. A topology's model is converted
from the very graph the host then executes, so ids match exactly. The agent's
graph is different: each session builds its own copy with fresh ids, so the
model published at start-up cannot carry them. There the feed falls back to the
node's name, and two nodes with one name (the first model call and its retry are
both `act`) are told apart by the order they first run in. Tests:
`a_copy_of_the_graph_with_other_ids_is_matched_by_name` and
`two_nodes_with_one_name_light_in_the_order_they_first_run`.

## The Logs tab is gone; the log capture is not

`LogPanelPlugin` did two jobs: it showed a tab, and it installed the global
`tracing` subscriber over the `LogBuffer` that feeds the conversation panel's
`Ctrl-O` node timeline. The default shell keeps the second and drops the first
(`LogPanelPlugin::without_panel`). `LogPanelPlugin` and `LogPanel` stay public
for other hosts. `tabs::add_shell_tabs` is the one place that lists the tabs;
`shell_tabs.rs` asserts the set and that `Ctrl-O` still shows a node's own log
lines.
