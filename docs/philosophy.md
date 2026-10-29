---
title: Polaris Philosophy (shared reference)
---

# Polaris Philosophy

`polaris_code` is a coding agent composed from Polaris (`polaris-ai`),
`polaris-tui`, and `polaris_ontrace`. It inherits the framework's design
philosophy wholesale. That philosophy is **single-source**: it is not forked
per repo, so every Polaris-family crate is held to the same principles.

**Canonical source:**
[`polaris/docs/philosophy.md`](https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md)
Read it in full before a design review; the summary below is orientation, not a
substitute.

## The principles this repo is reviewed against

- **ECS-inspired state and behavior** — state lives in shared *resources*,
  behavior in *systems* (pure functions declaring the resources they need).
  Components stay composable, replaceable, and testable in isolation.
- **Graph-based execution** — the graph *is* the agent; nodes are swappable and
  topology is verified before execution.
- **Plugin architecture** — every capability is delivered through plugins;
  nothing is mandatory, everything is replaceable, plugins compose by exposing
  resources and APIs without reaching into each other's internals.

For how these map onto *this* repo's own layers — runtime, UI, knowledge, and
the coding-specific tools/agents that glue them together — see
[`taxonomy.md`](./taxonomy.md).
