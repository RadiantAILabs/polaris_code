---
title: Polaris Philosophy in polaris_code
---

# Polaris Philosophy

`polaris_code` is a coding agent built on Polaris (`polaris-ai`), with
`polaris-tui`, `polaris_ontrace`, and `polaris_cosmos` in this workspace. It
inherits the framework's design philosophy: **everything is a plugin, and agent
behavior is composed from small, replaceable parts.** This page explains what
that means for work in this repository.

**Canonical source:**
[`polaris/docs/philosophy.md`](https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md)

The upstream document owns the shared philosophy; this page applies it to the
coding agent. Read both before a design review. The public workspace consumes
Polaris through the single crates.io dependency declared in the root
`Cargo.toml`; the other components are workspace crates.

## Make agent design easy to change

Building a capable coding agent requires discovering how it should behave for
a particular task, then changing that design as evidence arrives. The
architecture should shorten the path from a design hypothesis to an observable
result. A new tool, context policy, model provider, or execution topology should
be possible to try without redesigning the rest of the application.

Polaris supplies primitives without prescribing an execution loop.
`polaris_code` supplies a reflective ReAct agent as one concrete assembly, plus
task-specific graphs. Those designs can evolve; they do not become assumptions
that every capability must know about.

## Everything is a plugin

The `Server` orchestrates plugins. Models, tool execution, memory, persistence,
I/O, instrumentation, and agent registration enter the application through
plugins. Each plugin has a narrow responsibility and publishes resources or
APIs that other components consume through declared contracts. Higher-level
capabilities compose lower-level ones without reaching into their internals.

Here, `CodeToolsPlugin` contributes sandboxed tools to the tool registry,
`CodeGraphPlugin` adds code navigation over ontrace's store, and
`CodeAgentPlugin` registers the reflective agent. `CodingTopologyPackPlugin`
contributes graphs to the shared Cosmos catalog. The interactive conversation
panel and `ExecIOPlugin` are alternative providers of the same `UserIO`
contract, so the agent can run through either front end.

Choose capabilities when assembling the application. A plugin can be replaced
or omitted when the remaining assembly's declared requirements are satisfied;
consumers must still receive the resources and APIs they require. Small, proven
compositions can themselves become reusable building blocks.

Plugin composition is the capability boundary. Helpers and individual graph
systems remain ordinary functions and modules within that boundary. Keep
`main.rs` focused on configuration, registration, and host lifecycle; tool
behavior and agent decisions belong in their owning modules.

## Separate state from behavior

Following Polaris's ECS-inspired model, state lives in typed resources and
behavior in systems that declare their inputs. Use `Res<T>` for resource
reads, `ResMut<T>` for mutable local state, and `Out<T>` for a preceding
system's result. Shared global resources and isolated local contexts make the
ownership and lifetime of state explicit.

Prefer typed step-to-step results over a catch-all state struct filled with
optional intermediate values. Keep resources scoped to the consumers that need
them: a tool should consume its own capability's contracts, not the agent's
`LoopState`. Explicit inputs make systems easier to inspect, replace, and test
in isolation. See [`polaris-consumer.md`](polaris-consumer.md) for the data-flow
patterns.

## The graph is the agent

Express agent control flow as a directed graph of systems. Nodes perform work;
edges express sequencing, branching, looping, parallel work, and error paths.
Changing a design means replacing nodes or composing their edges, with topology
and typed connections checked before execution.

Keep reusable systems independent of one particular topology. Coding graphs
enter through topology packs and the shared catalog; their declared boundaries
and live host availability determine admission. Adding a graph belongs in its
pack, not in a special branch of the host's selection loop. See
[`topology-selection.md`](topology-selection.md) and
[`gate-planning.md`](gate-planning.md) for how this application composes and
runs them.

## Keep decisions inspectable and experiments testable

The reflective agent reads a per-turn `PolicySnapshot` and records its decisions
and tool calls in the Session `ABox`. Preserve those decision seams when
changing behavior. Topology selection and execution likewise retain their
admission, selection, and outcome evidence, so a design can be assessed from
what actually happened.

Test a system or plugin with the resources its contract requires, and test a
composition at its boundaries. An alternate provider, front end, or topology
should be usable without coupling the experiment to unrelated internals.

## Applying the philosophy to a change

- Add a capability as a self-contained module and plugin; extend existing
  registries and APIs instead of teaching other components its implementation.
- Declare what the plugin provides and requires. Document its resources, APIs,
  permissions, and lifecycle using the [plugin](reference/plugins.md),
  [API](reference/api.md), and [resource](reference/resources.md) standards.
- Keep the composition root responsible for wiring. Keep coding behavior here,
  UI presentation in `polaris-tui`, runtime mechanics in Polaris, and
  domain-neutral admission and selection contracts in Cosmos.
- Ask whether the change lets a component be composed, replaced, or tested
  independently. Requiring unrelated components to understand its internals is
  a reason to reconsider the boundary.

[`taxonomy.md`](taxonomy.md) maps these principles to this repository's layers.
The [integration guide](reference/guide.md) and
[plugin catalog](../crates/polaris_code/src/docs/plugins.md) show the
existing building blocks to compose before adding another one.
