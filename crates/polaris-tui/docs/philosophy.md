---
title: Polaris Philosophy
---

# Polaris Philosophy

`polaris-tui` follows the Polaris design philosophy, which is documented in the
Polaris repository:
[`docs/philosophy.md`](https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md).
In short:

## Principles

- **ECS-inspired state and behavior** — state lives in shared *resources*,
  behavior in *systems* (pure functions declaring the resources they need).
  Components stay composable, replaceable, and testable in isolation.
- **Graph-based execution** — the graph *is* the agent; nodes are swappable and
  topology is verified before execution.
- **Plugin architecture** — every capability is delivered through plugins;
  nothing is mandatory, everything is replaceable, plugins compose by exposing
  resources and APIs without reaching into each other's internals.

For how these map onto *this* crate's own layers — the TUI shell/runtime, the
panel/widget abstractions, and the plugin-contributed panels — see
[`taxonomy.md`](./taxonomy.md).
