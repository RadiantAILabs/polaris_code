A catalog of every [`Plugin`](polaris::system::plugin::Plugin) shipped by
`polaris_code`, grouped by the layer it occupies (see
[`taxonomy.md`](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/taxonomy.md)).

Each entry links to the plugin's rustdoc page, which is the canonical source for
its tools, resources, dependencies, lifecycle, and a registration example per
the
[plugin documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/reference/plugins.md#documentation-standard).
See the [integration guide](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/reference/guide.md)
to go the other way — from a goal to the plugins that satisfy it.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/plugin_catalog.rs`) that scans `src/` for `impl Plugin for X` and
> asserts each name appears below. Adding a new plugin without listing it here
> will fail CI.

# Layer 2 — Coding capabilities

Swappable capabilities contributed as plugins.

| Plugin | What it enables |
|--------|-----------------|
| [`CodingTopologyPackPlugin`](crate::cosmos::topology::CodingTopologyPackPlugin) | Contributes all seven coding graphs as atomic, admission-ready registrations to the one [`TopologyCatalog`](polaris_cosmos::topology::TopologyCatalog). It extends the catalog capability during `build`; adding a topology changes the pack, not host assembly. |
| [`DefaultLoopTopologyPackPlugin`](crate::cosmos::default_loop::DefaultLoopTopologyPackPlugin) | Publishes the default conversational agent loop as a catalog registration, under its own `polaris-code.conversational-turn` contract — the coding contract cannot admit it. Registers the graph [`CodeAgent`](crate::agents::CodeAgent) builds, so the loop keeps one definition. Publication, not activation: nothing requests that contract and the registration carries no author priors. |
| [`CodingTaskQueuePlugin`](crate::cosmos::host::CodingTaskQueuePlugin) | Publishes the host's [`CodingTaskQueue`](crate::cosmos::host::CodingTaskQueue) so the run surface's selection point can drain one accepted coding task per turn. Without it — or with an empty queue — [`select_and_run`](crate::cosmos::host::select_and_run) is inert and every turn stays conversational. |
| [`CodeToolsPlugin`](crate::CodeToolsPlugin) | The sandboxed [`CodeTools`](crate::CodeTools) toolset: `read_file`, `read_span`, `edit_file` (exact string-replace with uniqueness check + unified-diff preview), `create_file` (a new file at a path that does not exist yet; refuses to clobber), `search`, `list_files`. Constrained to the configured `working_dir`; `edit_file` and `create_file` gated `ToolPermission::Confirm` as one and journaled for `/rewind`. |
| [`CodeGraphPlugin`](crate::CodeGraphPlugin) | The deterministic Rust→triples indexer plus SPARQL-backed [`CodeGraphTools`](crate::CodeGraphTools) (symbol/def/ref queries) over the Code `ABox`. Depends on ontrace's `OxigraphPlugin`. |
| [`MemoryToolsPlugin`](crate::tools::MemoryToolsPlugin) | Reflective memory tools: memoize/recall conclusions, content-hash invalidated. |
| [`TaskToolsPlugin`](crate::tools::TaskToolsPlugin) | The [`TaskTools`](crate::tools::TaskTools) toolset — `run_in_background`, `task_output`, `stop_task` — over the [`ShellExecutor`](polaris::shell::ShellExecutor) `ShellPlugin` published and the host's [`BackgroundTasks`](crate::terminal::BackgroundTasks) table. It is the model's half of the same surface the `Ctrl-T` chord and the four `/commands` give the user: one process table, one task table, two ways in. `run_in_background` and `stop_task` are gated exactly as `run_command` is (see `SHELL_EXECUTION_TOOLS`), so backgrounding is never a route around a `Confirm`. |
| [`ProgressMiddlewarePlugin`](crate::ProgressMiddlewarePlugin) | Graph middleware that bridges execution instrumentation to the TUI conversation panel. |
| [`ExecIOPlugin`](crate::exec::ExecIOPlugin) | Registers `UserIO` over a channel-backed [`ExecIOProvider`](crate::exec::ExecIOProvider) for the non-interactive `--print` front end — installed instead of the TUI panels, never alongside them. |

`CodeGraphPlugin` keeps that one plugin and tool surface across Rust, Python,
JavaScript, TypeScript, and TSX. Its embedded non-Rust grammars add syntactic
navigation facts to the same Code `ABox`; the curated Polaris role/resource
queries remain Rust-only.

Its lifecycle is split deliberately. `build` registers the toolsets *and*
asserts that an index is pending, into a named graph of its own
(`urn:polaris:graph:code-index`) that a full re-index does not clear; `ready`
spawns the boot index, which flips that graph to `ready` only once the
enrichment pass has run too. Every graph tool reads those facts before it
answers, so a query that arrives during startup is told to wait rather than
handed an empty result, and [`terminal::status`](crate::terminal::status) puts
the same phase on the workspace row. No new resource or API carries this: the
index's state is asserted into the store both the tools and the agent already
hold. See [`tools::code_graph`](crate::tools::code_graph) for the freshness half
and its accepted failure mode.

`InertModelPlugin` (private, `crate::cosmos::compare`) is not part of the
public plugin surface, so it has no rustdoc page to link: a topology
comparison (`compare_intents`) stands up its own isolated, single-use server
per arm, and this plugin only exists so that server's `ModelsPlugin` has a
registered provider to resolve the comparison's unreachable placeholder model
id against. Every comparison task is pre-accepted and hermetic, so the
provider is never actually called. Listed here only to satisfy the drift guard
above.

# Layer 3 — Agent

The reflective coding agent.

The agent ships a guidance text that steers its tool choices toward the graph
tools. That text names the languages the code index covers, so
`outline`/`read_symbol` are recommended for every indexed language rather than
for Rust alone, and it records that the curated
`concepts`/`find_by_role`/`resources`/`accessors_of` queries describe
Rust/Polaris roles only. `tests/prompt_language_reach.rs` fails if the text and
the indexer's language set drift apart.

| Plugin | What it enables |
|--------|-----------------|
| [`CodeAgentPlugin`](crate::CodeAgentPlugin) | Registers the reflective [`CodeAgent`](crate::CodeAgent): a ReAct loop whose control decisions read a per-turn [`PolicySnapshot`](crate::PolicySnapshot) and write a reified `Decision`/`ToolCall` into the Session `ABox`. Depends on `SessionsPlugin`. |
