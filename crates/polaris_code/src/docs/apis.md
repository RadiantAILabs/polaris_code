A catalog of every [`API`](polaris::system::api::API) shipped by
`polaris_code`. APIs are the typed handles plugins publish so other plugins
can contribute to or drive them at wiring time.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans `src/` for `impl API for X` and asserts
> each name appears below. Adding a new API without listing it here will fail
> CI.

_none_ — `polaris_code` publishes no APIs of its own. Its plugins register
tools and resources and consume upstream APIs (`SessionsAPI`, `ToolRegistry`,
the TUI `TuiRegistry`) rather than exposing new ones. If that changes, add the
API here and document it per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/reference/api.md#documentation-standard).

The embedded Python, JavaScript, TypeScript, and TSX indexers add no API: they
populate the existing Code `ABox` behind `CodeIndexer` and `CodeGraphTools`.

Permission authorization exposes ordinary typed functions rather than a framework
`API`: [`PermissionPolicy::resolve_call`](crate::agents::PermissionPolicy::resolve_call)
returns an explained deterministic verdict, then
[`ShellApproval::resolve`](crate::agents::permission_rules::ShellApproval::resolve)
applies shell hard denies and opacity checks. A reviewer may consume a remaining
confirmation before the host constructs
[`ApprovalRequest`](crate::agents::permission_rules::ApprovalRequest), unless the
verdict is marked
[`HumanOnly`](crate::agents::permission_rules::ReviewEligibility::HumanOnly).
Capabilities state facts about their tools (read-only or mutating, and whether
every attended call needs a person) in the
[`ToolPolicyRegistry`](crate::tools::ToolPolicyRegistry) resource; this is a
registry the capability plugins fill while they build, not a framework `API`.
Plugins add rules written in Rust the same way, with
[`PermissionRules::add_code_rule`](crate::agents::permission_rules::PermissionRules::add_code_rule)
while they build; the rules are fixed once the server is ready.

Context compaction also extends existing seams without adding an API:
`ContextStrategy` selects projection behavior, `ContextManager` stores summary
checkpoints, and the command registry carries `/compact` to the agent graph as
a typed [`AgentRequest`](crate::terminal::AgentRequest) that
`CompactionPlugin` registers next to the agent that reads it.

The MCP client adds no API: it extends the upstream `ToolRegistry` and the
`ToolPolicyRegistry`, and exposes typed connection status through
[`McpRegistry`](crate::tools::mcp::McpRegistry).
[`WebToolsPlugin`](crate::tools::WebToolsPlugin) likewise extends `ToolRegistry`
and `ToolPolicyRegistry` without adding an API; its per-call client validates
and pins destinations.

[`AutoReviewPlugin`](crate::agents::auto_review::AutoReviewPlugin) likewise
composes through typed resources and explicit graph output rather than a new
framework API. Its `ReviewPolicy` configures the graph reviewer; `ReviewResult`
is consumed at dispatch only after deterministic authorization is rechecked.
Reviewer calls use the existing `ModelRegistry` and `TokenMeter` contracts.

Startup configuration and `/status` add no API. The binary applies TOML defaults
through clap and registers diagnostics through the existing
[`CommandRegistry`](crate::terminal::CommandRegistry).

The shell approval contract is also an ordinary typed boundary:
[`ApprovalReceipt`](crate::tools::approval::ApprovalReceipt) is host-issued context
with exact arguments, directory binding, host policy validation and single use.
[`ApprovalShellTool`](crate::tools::approval::ApprovalShellTool) consumes it inside
registered `run_command` execution. Alternative hosts can issue receipts with their
own policy validator; tools have no dependency on agent permission state.

Where a call works is the same kind of boundary. [`CallRoot`](crate::tools::CallRoot)
is a tool-context value, not an API or a resource: the agent's tool lanes insert
the session's working directory for every call. The file tools (`read_file`,
`read_span`, `edit_file`, `create_file`, `list_files`, `search`, `glob`),
`run_command` and `run_in_background` follow it, and a toolset used without one
keeps the directory it was built with. A background task carries the root it
started under: `task_output` and `stop_task` called with a root see only tasks
with that same root, and a host call without one sees all of them. The
code-graph, memory and MCP tools do not read it and stay server-wide, on the
directory, index and connections the server's plugins were built on. The root is
where a shell command starts and what a `working_dir` must stay inside; it is not
an operating-system sandbox for what the command then does.

A session's shell command patterns
([`AgentConfig::command_globs`](crate::agents::AgentConfig::command_globs)) are
checked when each call is authorized, so they too belong to the session and not
to the server. A session with patterns lists them in its system prompt and in
each refusal, so the model writes commands that match, and a session with an
empty list is not offered `task_output` or the other shell tools. Neither adds an
API: for the tools above, two sessions on one server differ only in the
`AgentConfig` they were created with, and in the `TokenMeter` when one of them
carries a spend cap.
