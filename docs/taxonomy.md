---
title: polaris_code Taxonomy
---

# polaris_code Taxonomy

This document defines the conceptual layers and core abstractions in
`polaris_code`. For the design philosophy these layers serve, see
[philosophy.md](./philosophy.md), which applies the shared Polaris philosophy
to this application and links to its canonical upstream source.

`polaris_code` is a terminal coding agent that composes Polaris (runtime),
`polaris-tui` (UI), `polaris_ontrace` (knowledge), and `polaris_cosmos`
(cognition), then adds coding-specific tools and a reflective agent. Polaris
comes from crates.io; the other components live in this workspace. The
application's layers follow the Polaris rule — **lower layers are the fixed
composition and capability seams, while the upper layer is the agent design.**

## Architecture

| Layer | Name | Owns |
|-------|------|------|
| **3** | Agent & Policy | the reflective `CodeAgent`, its per-turn `PolicySnapshot` read seam, and the reified `trajectory` write seam |
| **2** | Coding Capabilities | the coding toolsets and graph instrumentation, each a self-contained plugin |
| **1** | Composition Root | the binary that boots the `Server` and wires runtime + UI + knowledge + cognition under one `polaris-ai`, plus cross-cutting session resources |

## Layer 1: Composition Root

**Files:** `src/main.rs`, `src/lib.rs`, `src/usage.rs` (`TokenMeter`),
`src/config.rs` (`ModelCatalog`), `src/sparql.rs`,
`src/run.rs` (`drive_turn`/`drive_until`, `persist_turn`,
`stop_signal`/`shutdown_driver`),
`src/cosmos/host.rs` (the selection point's lifecycle); and the dependency
wiring in the workspace `Cargo.toml`.

The fixed scaffolding the rest of the app stands on:

- **The boot wiring.** `main.rs` constructs a `polaris::Server` and registers
  the upstream plugins (sessions, models/Anthropic, the polaris-tui shell,
  ontrace's `OxigraphPlugin`) plus this crate's own plugins, then picks a
  front end — interactive or `--print` — that drives `src/run.rs`'s shared
  turn functions. Layer 3's agent graph is identical either way. The
  resilience each front end needs around that loop — saving after every turn,
  stopping on SIGINT or SIGTERM, deciding whether a failed turn goes back to
  the prompt — is also `src/run.rs`'s, as free functions the front end
  composes; no host-side type owns the loop, the session identity and the
  signal handling together.
- **The single-`polaris-ai` rule.** Every Polaris-family dependency must resolve
  to one `polaris-ai` source or shared types won't unify. This is a fixed,
  load-bearing constraint of the composition — see `AGENTS.md`.
- **Cross-cutting session state.** `TokenMeter` (global), the
  `CodingTaskQueue` of accepted coding tasks (global), and shared SPARQL
  helpers used across capabilities.
- **The topology-selection point.** `run.rs` runs `cosmos::host::select_and_run`
  inside the outer session loop, before any graph starts. It owns run
  identities, pristine context construction, `GraphExecutor` invocation,
  evidence appends and outcome verification; Cosmos owns admission and
  selection semantics. The fixed order is documented in
  [`topology-selection.md`](topology-selection.md); a layer that merely *adds a
  topology* must not change it.

**Scope.** Layer 1 owns wiring and invariants, not behavior. It picks *which*
capabilities and surfaces are present; it does not implement tool logic or agent
control flow.

## Layer 2: Coding Capabilities

**Files:** `src/tools/code.rs` (`CodeToolsPlugin`), `src/tools/code_graph.rs`
(`CodeGraphPlugin`), `src/tools/memory.rs` (`MemoryToolsPlugin`),
`src/progress.rs` (`ProgressMiddlewarePlugin`), `src/terminal/`
(`CommandRegistry`, `CommandDispatch`, `WorkspaceFiles`, `LiveTranscript`),
`src/exec/` (`ExecIOPlugin`).

The swappable capabilities this app contributes as Polaris plugins:

- **Toolsets.** Sandboxed file tools (`CodeTools`), the deterministic Rust
  indexer + SPARQL-backed symbol queries (`CodeGraphTools` / `ConceptTools`),
  reflective memory (`MemoryTools`), and the model's own route to a background
  shell run (`TaskTools`: `run_in_background`, `task_output`, `stop_task`),
  which composes the executor `ShellPlugin` publishes with the same
  `src/terminal/background.rs` table the user's chord writes.
- **Instrumentation.** `ProgressMiddlewarePlugin` bridges graph execution events
  to the conversation panel.
- **Terminal policy.** `src/terminal/` supplies what `polaris-tui`'s
  conversation panel deliberately leaves to its host: the `/command` set and
  its execution (`CommandRegistry`, run by the `CommandDispatch` `IOProvider`
  decorator before the agent's `receive` returns, so a command is never a
  model request), the `@` file-mention source with its ignore rules and
  working-directory containment (`WorkspaceFiles`, `mentions`), and the live
  message/tool block vocabulary the agent streams its answer and its tool
  activity through (`transcript`), and the table of managed shell runs — the
  ones the user pushed out of a turn with the chord and the ones the model
  started with `run_in_background` — with the four `/commands` that read it and
  the bounded ring of finished results the model reads back (`background`).
  Presentation stays in the TUI — the panel
  folds and draws blocks and knows nothing about what any tool does — and the
  layering holds in both directions: `transcript` names phases and identities,
  never colours or rows, and `background` keeps every process concept on this
  side of the boundary: the panel learns a count and a callback, never a task
  list, a process id type or a task state.
- **Front-end IO.** `ExecIOPlugin` registers `UserIO` over a channel-backed
  provider for the non-interactive front end, structurally identical to how
  `polaris-tui`'s `ConversationPanelPlugin` registers `UserIO` for the TUI.
  Its companion `exec::report` renders what that front end prints — the
  `--output-format` vocabulary, the identity triple and the exit code — so the
  composition root writes bytes but decides nothing about their shape.

**Scope.** Each capability is a self-contained module + plugin, added under
`src/tools/`. Tools stay sandboxed to the configured `working_dir`. A capability
must not reach into the agent's loop state — it exposes a tool or a resource and
nothing more.

## Layer 3: Agent & Policy

**Files:** `src/agents/code_agent.rs` (`CodeAgent`, `CodeAgentPlugin`,
`AgentLlm`), `src/agents/policy.rs` (`PolicySnapshot`),
`src/agents/trajectory.rs`, `src/agents/project_context.rs` (`ProjectContext`),
`src/agents/context.rs` (`ContextManager`), `src/agents/state.rs` (`LoopState`),
`src/agents/config.rs` (`AgentConfig`).

The agent design:

- **The loop.** `CodeAgent` is a reflective ReAct loop; each control decision
  *reads* a per-turn `PolicySnapshot` and *writes* a reified `Decision` /
  `ToolCall` into the session `ABox` (`trajectory`, write-only provenance).
- **The seams.** `policy` is the read seam every decision consults; `trajectory`
  is the write seam. Grounding comes from `ProjectContext` + seed ontologies;
  `ContextManager` curates the message/`ABox` window.
- **Grounding's own reach and refresh** — which `AGENTS.md`/`CLAUDE.md`
  documents are found, in what precedence order, under what size budget, and
  which half of `ProjectContext` re-reads per turn — are documented in
  [`project-instructions.md`](project-instructions.md).

**Scope.** This layer is meant to change often. The read-policy / write-trajectory
discipline is the invariant that keeps experimentation legible — decisions must
flow through the seams, not hardcode behavior.

## Boundary Guide

When adding functionality, place it by asking:

| Question | If yes | Examples |
|----------|--------|----------|
| Is it wiring, a framework-plugin choice, or a composition invariant? | **Layer 1** | Registering another framework plugin, a new global session resource, dependency wiring |
| Is it a new tool, toolset, or instrumentation surface? | **Layer 2** | A new `#[toolset]` + plugin, a new middleware bridge |
| Is it a change to how the agent decides, grounds, or records? | **Layer 3** | A new decision seam, a policy field, a trajectory predicate, a context-window rule |

Lower must not depend on upper: the composition root names capabilities and the
agent, not the reverse; a Layer-2 tool must not read the agent's `LoopState`;
the agent reaches capabilities through the tool registry, not their internals.

## Multi-language indexing boundary

The Layer-2 code graph capability normalizes Rust, Python, JavaScript,
TypeScript, and TSX syntax into one session-local Code `ABox` behind the
existing navigation tools. Parser selection and language-specific extraction
remain encapsulated by that capability. Rust/Polaris conceptual-role
enrichment remains a separate derived graph and never infers roles from
non-Rust syntax.
