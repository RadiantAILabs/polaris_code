A catalog of every consumer-facing [`GlobalResource`](polaris::system::resource::GlobalResource)
and [`LocalResource`](polaris::system::resource::LocalResource) shipped by
`polaris_code` — the types a `#[system]` consumes via `Res<T>`.

Each entry links to the resource's rustdoc page, which documents its scope,
provider, access pattern, and a real-shape consuming system per the
[resource documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/reference/resources.md#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/resource_catalog.rs`) that scans `src/` for `impl GlobalResource for
> X` / `impl LocalResource for X` and asserts each name appears below — unless
> it is listed in that test's `INTERNAL_RESOURCES` allowlist (per-turn agent
> plumbing not part of the consumer surface). Adding a new consumer-facing
> resource without listing it here will fail CI.

# Session accounting

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`TokenMeter`](crate::TokenMeter) | Global + Local | Running token counts (input / output / cache tiers) and an estimated USD cost; `summary(model_id)` formats the exit report. The session's meter is global, because it accrues across the whole session. The same type is also local: the host inserts a fresh meter into each topology run context, where it shadows the global one, so a graph that calls a model bills the run and leaves the session's total alone. |
| [`RunCeiling`](crate::RunCeiling) | Global | The operator's spend limit for the run — money, tokens, or both — installed only when one was asked for, so its *absence* means no limit was set. Decides nothing itself: `evaluate(&RunUsage)` reports a [`CeilingStatus`](crate::usage::CeilingStatus) and [`run::drive_turn`](crate::run::drive_turn) consults it on the turn boundary. Global for the same reason the meter is: the limit bounds the whole run, not one turn. |
| [`TaskCostLimit`](crate::usage::TaskCostLimit) | Global | The validated USD limit installed when explicitly set or when the lean arm supplies its $1.50 default. Each accepted coding task gets a fresh local meter and reads this limit before its model calls; the same limit applies to each task independently. |

# Model provider

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`ProviderReport`](crate::providers::ProviderReport) | Global | What the provider that passed the launch check is running as: which provider, the AWS region or custom endpoint when there is one, and where its credentials came from — a variable name, stored login key, or an AWS source, never the credential. `/status` reads this launch report directly. `line()` renders the one-line form a session opens with. Installed by the binary from [`ReadyProvider::report`](crate::providers::ReadyProvider::report); a host that builds its own server installs none. Global because one provider serves every session in the process. |

# Safety

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`McpRegistry`](crate::tools::mcp::McpRegistry) | Global | Configured MCP servers, typed [`McpStatus`](crate::tools::mcp::McpStatus) connection health, namespaced tools, project servers waiting for trust, and sanitized startup failures. Provided by `McpClientPlugin`; `/mcp` reads the same projection. |
| [`SecretRedactor`](crate::agents::code_agent::SecretRedactor) | Global | The rule set that withholds recognisable credentials from tool output before it reaches the model, the transcript, the log or the session graph. `scan(text)` returns the rewritten text plus which shapes fired; `execute_tools` and `execute_read_only_batch` apply it where a result becomes a string, which every consumer of a tool result is downstream of. Global because the rule set is process configuration chosen once at the composition root, not per-session state. Provided by [`CodeAgentPlugin`](crate::agents::CodeAgentPlugin) and gated by `POLARIS_CODE_REDACT`. **Not a permission boundary** — see [`docs/secret-redaction.md`](https://github.com/RadiantAILabs/polaris_code/blob/main/docs/secret-redaction.md). |

# Terminal presentation

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`LiveTranscript`](crate::terminal::transcript::LiveTranscript) | Local | The assistant message block currently being streamed to the terminal: mint a block with `open`, forward text through the [`MessageHandle`](crate::terminal::transcript::MessageHandle) it returns, and `close` it with the authoritative text. Local because a block belongs to one session's turn. A host that renders the agent elsewhere reads the same `io_type = "message"` / `"tool"` vocabulary off the `UserIO` channel and needs no resource at all. |
| [`TurnSteering`](crate::terminal::TurnSteering) | Local | Ordered interactive follow-ups and active-run steering. The existing run driver activates and finishes run identities; the agent drains steering only at its loop boundary. Also the containment boundary for the submissions it holds: it carries [`ContainedPrompt`](crate::terminal::ContainedPrompt)s, so every submission's `@` mentions are resolved against the working directory the agent binds into it before the queue will take it. Disabled by default for non-terminal consumers. |
| [`BackgroundTasks`](crate::terminal::BackgroundTasks) | Local | Managed shell runs the user pushed out of a turn: which [`ShellProcessId`](polaris::shell::ShellProcessId) belongs to which tool block, the bounded tail of each one's output, the one flag the `Ctrl-T` chord sets, and — once backgrounded — the invocation's own wait, a [`ToolRun`](crate::terminal::background::ToolRun). `execute_tools` fills it through the [`TaskSink`](crate::terminal::background::TaskSink) it installs in the tool context, races the wait against it, and hands the wait over on the chord; [`BackgroundCommands`](crate::terminal::BackgroundCommands) reads it back and [`BackgroundReporter`](crate::terminal::BackgroundReporter) takes the wait to its end. The wait is carried rather than dropped because a `run_command` run is reserved for its `execute` caller — the executor refuses to hand its result to anyone else — and that caller's handle stops the child when dropped. Local, and cloned into every session the host opens, because a task's *provenance* is one session while the run itself outlives it — the process belongs to the server-scoped [`ShellExecutor`](polaris::shell::ShellExecutor), which is what makes a task survive a turn ending and a session switch. A consumer that wires no terminal gets one that reports to nobody. |
| [`AttachmentStore`](crate::terminal::AttachmentStore) | Local | Validated composer attachments, parked between the moment the user attaches one and the turn whose prompt names its handle. The terminal's [`WorkspaceAttachments`](crate::terminal::WorkspaceAttachments) fills it; `receive_user_input` redeems handles out of it, once each, into the image blocks of one canonical `Message::User`. Local because a handle belongs to one session's host. A consumer that wires no terminal gets an empty store and never asks it for anything. |

# Cosmos coding embodiment

Resources a host installs into the live `SystemContext` before it executes a
selected coding topology. Preparing that context is the embodiment's job, not
Cosmos's, so these are the concrete surface a consumer must provide — and each
is declared to Cosmos's admission boundary by
[`CodingTopologyPackPlugin`](crate::cosmos::topology::CodingTopologyPackPlugin)
as part of each complete registration.

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`CodingWorkspace`](crate::cosmos::payload::CodingWorkspace) | Local | The sandboxed working tree one coding topology acts through: read, any number of exact-string edits, re-read verification, an attempt counter, and an ordered role trace. Holds no success check, so a graph running in the pristine context cannot reach a subprocess. Read as `Res<T>` because an edit mutates the filesystem rather than the record. |
| [`SweepWorklist`](crate::cosmos::payload::SweepWorklist) | Local | The bounded queue and ledger backing the embodiment-owned [`worklist_sweep_graph`](crate::cosmos::graphs::worklist_sweep_graph). Read as `ResMut<T>`; the separate coverage-audit topology reuses Cosmos's own explained private state. |
| [`CodingQuestion`](crate::cosmos::payload::CodingQuestion) | Local | The parsed question backing [`code.tree-question`](crate::cosmos::topology::TREE_QUESTION): one of the two shapes it can answer from the tree, or `Unsupported` for anything else. Read as `Res<T>`; prepared unconditionally, so a task that asked nothing carries an unanswerable question rather than no question. |
| [`ProjectTestRunner`](crate::cosmos::payload::ProjectTestRunner) | Local | A project test runner. Declared by [`code.test-gated-edit`](crate::cosmos::topology::TEST_GATED) and by the gate-plan blocks `code.run-project-tests`/`code.run-project-tests-captured`. A host installs one only for a turn whose accepted task states a command success check, bounded by that stated program, args, a timeout and an output cap; a task with no stated command leaves it uninstalled, which is what makes that candidate's `resource.missing` exclusion a real observation rather than a permanent one. `run` additionally keeps a small, independently bounded tail of the command's combined stdout/stderr ([`TestRunOutcome`](crate::cosmos::payload::TestRunOutcome)) alongside the verdict `judge` already reported, for a repair step to read back — captured best-effort and never itself why a run is judged an issue. |
| [`CodingTaskQueue`](crate::cosmos::host::CodingTaskQueue) | Global | The host's queue of accepted coding tasks. A front end clones the handle and [`accept`](crate::cosmos::host::CodingTaskQueue::accept)s into it; the run surface's selection point takes at most one entry per turn. Global because acceptance and selection happen in different parts of one host, across every live session. |
| [`CompositionTemplates`](crate::cosmos::composition::CompositionTemplates) | Global | Optional startup bindings for a separately owned coding registration: each names a finite versioned recipe, ordered Cosmos blocks, and a builder that rematerializes its native graph for every selected turn. The shipped coding pack's mappings are sealed in the embodiment; this resource exists only so an application pack cannot bypass composition/evidence by supplying an otherwise opaque catalog graph. Global because it is static application composition read by every run context. |
| [`CompositionPolicy`](crate::cosmos::composition::CompositionPolicy) | Global | The explicitly selected policy that chooses an approved finite composition before admission. Absent means the deterministic registration projection; a host installs `Model` deliberately to let the configured model select only from the supplied grammar. Global because one host configuration must govern every per-turn run without a hidden provider-triggered fallback. |

# Coding gate blocks

Resources a caller installs before materialising and running one or more
blocks from [`coding_block_catalogue`](crate::cosmos::gate_blocks::coding_block_catalogue)
directly, outside the shipped whole-graph coding registrations. Only a
capability a block cannot get any other way is a resource here: per-instance
configuration (a `code.model-propose` role, a `code.survey-paths` path list)
is decoded from the block's own `ResolvedConfigurationRef` at build time and
seeded into the graph as that instance's own small producing node, read
downstream as `Out<T>` — never an ambient `Res<T>` — which is what lets two
differently configured instances of the same block coexist in one graph. See
[`gate_blocks`](crate::cosmos::gate_blocks) module docs for why configuration
travels this way rather than as a factory parameter.

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`ModelStepProvider`](crate::cosmos::gate_blocks::configurable::ModelStepProvider) | Local | The installed [`ModelStepExecutor`](crate::cosmos::gate_blocks::configurable::ModelStepExecutor) `code.model-propose` calls — a live provider-backed implementation in a real run, or [`ScriptedModelStep`](crate::cosmos::gate_blocks::configurable::ScriptedModelStep) in a test, matching how `ProjectTestRunner`'s absence versus presence already decides whether `code.run-project-tests` can run. A live turn may additionally grant `list_dir` and `search` (bounded, read-only) alongside `read_file`, per the step's own [`ModelStepConfig::tool_subset`](crate::cosmos::gate_blocks::configurable::ModelStepConfig); its turn cap and, for a repair step wired after a captured test run, whether it reads that run's failure are likewise per-step configuration, not this resource. |
| [`ModelStepHandoff`](crate::cosmos::gate_blocks::configurable::ModelStepHandoff) | Local | A fresh record for one gate-plan run: bounded artifacts and tool outcomes for a `brief` step, plus the last opaque conversation for a `continue` step. Every model step reads its local `Res<T>` handle, snapshots it before its model call, then writes the result under a short lock; `ModelStepConfig::context_mode` decides what the step consumes. |
| [`CodingTask`](crate::cosmos::task::CodingTask) | Local | The accepted task read by `code.classify-request` and `code.request-clarification`. A caller composing either block directly must insert the same task used to make its [`CodingWorkspace`](crate::cosmos::payload::CodingWorkspace), so classification and the bounded clarification question refer to the same request. |

Per-turn agent state (`AgentConfig`, `AgentLlm`, `PolicySnapshot`,
`ProjectContext`, `ContextManager`, `LoopState`, `AttemptLedger`) is internal plumbing read only
by the agent's own systems and is intentionally excluded — see the
`INTERNAL_RESOURCES` allowlist in `tests/resource_catalog.rs`.

`ContextManager` persists both canonical messages and structured summary
checkpoints. Its graph node reads the existing session `TokenMeter` for cost
reservation and billing; compaction adds no separate resource. Summary focus
requests and token calibration remain runtime state.

The embedded multi-language index adds no resource. Parser state is private and
constructed on demand inside the existing background indexing task.

# Permission authorization

| Resource | Scope | What it exposes |
|----------|-------|-----------------|
| [`PermissionRules`](crate::agents::permission_rules::PermissionRules) | Global | Immutable parsed user/project rules, source paths, fail-closed errors and explicit project trust (this process, optionally remembered per project). Provided by `PermissionRulesPlugin`; read by `PolicySnapshot::load` and the argument-aware permission gate. |
| [`SessionGrants`](crate::agents::permission_rules::SessionGrants) | Global handle, session lifetime | Exact command/directory and decomposed prefix scopes explicitly selected by a human. Every access checks `SessionSettings::approval_generation`; session reset invalidates all prior scopes. Shared with `/permissions` for inspection and revocation. |
| [`ShellApproval`](crate::agents::permission_rules::ShellApproval) | Global | Agent-policy shell restriction resolution and registered-tool dispatch. Issues tool-layer receipts bound to the current rule revision and session generation; the registered shell tool consumes them, preserving hard denies and the foreground/background wait lifecycle. |
| [`RunToolGrants`](crate::agents::permission_rules::RunToolGrants) | Global | Immutable explicit launch-time `--allow-tool` names/prefixes, applied only to unattended policy. MCP requires an exact tool name. |
| [`ToolPolicyRegistry`](crate::tools::ToolPolicyRegistry) | Global | The facts each capability states about its tools: read-only or mutating, and whether every attended call needs a person. Provided by `ToolPolicyPlugin`; capability plugins declare their tools while they build, and agent policy reads the facts when it authorizes a call. |

## Automatic permission review

| Resource | Scope | Purpose |
|---|---|---|
| [`ReviewPolicy`](crate::agents::auto_review::ReviewPolicy) | Global | Reviewer model selection, timeout, output cap and repeated-denial limit, provided by `AutoReviewPlugin`. |

`AutoReviewPlugin` also keeps two crate-private local resources: the bounded
real user input captured at ingress (never reconstructed from model or tool
history) and the equivalent-denial counts that reset at each user turn. Nothing
outside the crate reads them, so they are not part of this catalog's surface.

[`ReviewResult`](crate::agents::auto_review::ReviewResult) is typed graph output,
not shared mutable session state. It carries per-call verdicts bound to exact
arguments, canonical directory, policy revision and settings generation, with
pending single-use receipts kept internal to the authorization path.

Startup configuration and `/status` add no resource. Diagnostics read the
existing [`SessionSettings`](crate::terminal::SessionSettings) and
[`SessionNavigation`](crate::terminal::SessionNavigation); loaded file paths and
credential source names remain immutable launch inputs owned by the binary.

[`ApprovalReceipt`](crate::tools::approval::ApprovalReceipt) is per-invocation
`ToolContext`, rather than an ECS resource. The host supplies policy validation;
foreground and background tools consume the same lower-layer contract.
