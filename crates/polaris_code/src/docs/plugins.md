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
| [`ToolPolicyPlugin`](crate::tools::ToolPolicyPlugin) | Publishes the [`ToolPolicyRegistry`](crate::tools::ToolPolicyRegistry), where a capability states facts about its tools (read-only or mutating, and whether every attended call needs a person). Agent policy reads those facts; a capability never decides a permission mode. Capabilities that declare facts, and the permission rules plugin, depend on it, so it is added automatically when missing. |
| [`McpClientPlugin`](crate::tools::mcp::McpClientPlugin) | Discovers configured stdio and Streamable HTTP servers, registers namespaced tools in the existing registry as `ToolPermission::Confirm`, declares each as needing a person in attended modes (and read-only when the user's own configuration declares its server `readOnly`), publishes connection status, and adds `/mcp` (including `/mcp trust`). A project's `.mcp.json` server starts only after the user trusts its exact declaration. Calls follow the host's permission and redaction paths, with timeouts, a 1 MiB HTTP input cap before parsing, a 1 MiB aggregate discovery metadata cap, and bounded displayed results. |
| [`ApprovalShellPlugin`](crate::tools::approval::ApprovalShellPlugin) | Extends framework `ShellPlugin` with registered receipt-aware foreground execution. Depends on the shell provider, which retains executor ownership and cleanup; preserves other tools and registry overrides. |
| [`TaskToolsPlugin`](crate::tools::TaskToolsPlugin) | The [`TaskTools`](crate::tools::TaskTools) toolset — `run_in_background`, `task_output`, `stop_task` — over the [`ShellExecutor`](polaris::shell::ShellExecutor) `ShellPlugin` published and the host's [`BackgroundTasks`](crate::terminal::BackgroundTasks) table. It is the model's half of the same surface the `Ctrl-T` chord and the four `/commands` give the user: one process table, one task table, two ways in. `run_in_background` and `stop_task` are gated exactly as `run_command` is (see `SHELL_EXECUTION_TOOLS`), so backgrounding is never a route around a `Confirm`. |
| [`WebToolsPlugin`](crate::tools::WebToolsPlugin) | The [`WebTools`](crate::tools::WebTools) toolset: `web_fetch`, which reads a public `http(s)` page and returns it as markdown (HTML) or text, with the final URL and content type, in pieces of at most 32 KiB that the last line pages through. Reaches public hosts only — loopback, private-network and link-local addresses are refused on every redirect hop, and the connection is pinned to the addresses that were checked. Redirects, download size and time are bounded. Registered `ToolPermission::Confirm` and declared as needing a person: every attended mode, `auto` included, asks (showing the URL), and neither an allow rule, a session grant nor the automatic reviewer removes the prompt. An unattended run refuses it unless it grants the tool by name. See [`docs/web-access.md`](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/web-access.md). |
| [`ProgressMiddlewarePlugin`](crate::ProgressMiddlewarePlugin) | Graph middleware that bridges execution instrumentation to the TUI conversation panel. |
| [`ExecIOPlugin`](crate::exec::ExecIOPlugin) | Registers `UserIO` over a channel-backed [`ExecIOProvider`](crate::exec::ExecIOProvider) for the non-interactive `--print` front end — installed instead of the TUI panels, never alongside them. |
| [`StoredKeyOpenAiPlugin`](crate::providers::StoredKeyOpenAiPlugin) | Registers the `openai/<model>` provider with a key `polaris login` stored, for a run whose environment holds none. The framework's [`OpenAiPlugin`](polaris::models::OpenAiPlugin) reads its key only from an environment variable; this plugin registers the same public provider with the key it is given, and is added *instead of* [`OpenAiPlugin`](polaris::models::OpenAiPlugin), never alongside it. [`providers::prepare`](crate::providers::prepare) and [`ReadyProvider::install`](crate::providers::ReadyProvider::install) choose between them. |

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
| [`CodeAgentPlugin`](crate::CodeAgentPlugin) | Registers the reflective [`CodeAgent`](crate::CodeAgent): a `ReAct` loop whose control decisions read a per-turn [`PolicySnapshot`](crate::PolicySnapshot) and write a reified `Decision`/`ToolCall` into the Session `ABox`. Depends on `ModelsPlugin`, `ToolsPlugin` and `PersistencePlugin`, and on `PermissionRulesPlugin` and `AutoReviewPlugin`, which are added with their defaults when missing. |
| [`PermissionRulesPlugin`](crate::agents::permission_rules::PermissionRulesPlugin) | Loads immutable user/project allow, ask and deny rules; publishes scoped human grants, launch-time tool grants and exact-call shell authorization; registers `/permissions` (`trust-project` can be remembered per project through `with_project_trust`). `PermissionRulesPlugin::default()` loads no files, so an embedding that omits `new(root)` also omits the operator's deny rules. |
| [`AutoReviewPlugin`](crate::agents::auto_review::AutoReviewPlugin) | Installs bounded attended review configuration and real-user intent/denial state. The explicit graph review step uses the model registry and shared token meter, defers failures to human approval, and leaves deterministic policy and exact-call receipts authoritative. A call that only a person may approve (an `ask` rule, an untrusted project allow rule, a tool declared as needing a person) never reaches the reviewer. Requires `ModelsPlugin` and `PermissionRulesPlugin`. |
| [`CompactionPlugin`](crate::agents::context::CompactionPlugin) | Adds `/compact`, which asks the agent graph for a summary checkpoint now. It requires `CodeAgentPlugin`, the consumer of the typed [`AgentRequest`](crate::terminal::AgentRequest) it makes, so the command is advertised only where the agent that reads it is installed. Without it the agent still summarizes automatically past its threshold. |

`CodeAgentPlugin` composes [`SummarizingWindow`](crate::agents::context::SummarizingWindow)
with the existing cache-aware or sliding strategy. Its graph creates metered
summary checkpoints after cheap elision, bounded by the retry policy's
`attempt_timeout`, and `CompactionPlugin` serves `/compact` through the same
node without a normal answer call. Canonical history and checkpoints persist
together; see [context compaction](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/context-compaction.md).

The agent graph's model call is itself staged: `act` is one attempt, and a
failed one is retried or given up on by graph edges that consult
[`RetryPolicy`](crate::agents::policy::RetryPolicy). That adds no plugin; the
stages and their state are private to the agent.

Startup configuration and `/status` use the existing terminal command registry;
they add no plugin. The binary loads optional user/project TOML files before
assembly and registers read-only diagnostics over its current session handles.
See [startup configuration](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/configuration.md).
