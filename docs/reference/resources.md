# Resource Documentation Standard

A resource is typed state a `#[system]` consumes via `Res<T>` / `ResMut<T>`.
`polaris_code` exposes one consumer-facing global — `TokenMeter` (session token
accounting) — while its per-turn agent state (config, policy snapshot, loop
state, context window, LLM handle) is implementation plumbing read only by the
agent's own systems. This page is the workspace's normative standard; it
adapts the canonical
[Polaris resource standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/resources.md#documentation-standard).

## Documentation Standard

A consumer-facing resource's rustdoc MUST include the following. Flag an ISSUE
if any required section is missing.

| Section | Content |
|---------|---------|
| Purpose + when-to-use | What capability the resource exposes **from a system author's perspective**. |
| `# Scope` | `Global` or `Local`, **and one sentence of justification** (e.g. `TokenMeter` is global because token usage accrues across the whole session). |
| `# Provided By` | The plugin or boot wiring that registers it. |
| `# Access` | Valid parameter types (`Res<T>` only, or `Res<T>` + `ResMut<T>`) and the mutation contract; for globals, any interior mutability. |
| `# Alternatives` | Related/variant resources, or a `_none_` row. |
| `# Example` | A real-shape `#[system]` consuming the resource. |

### Internal resources

Per-turn agent plumbing (`AgentConfig`, `AgentLlm`, `PolicySnapshot`,
`ProjectContext`, `ContextManager`, `LoopState`) is **not** a downstream
consumer surface — it is built in the agent's `setup` and read only by the
agent's own `#[system]`s. Such resources are exempt from this standard; they are
listed in the `INTERNAL_RESOURCES` allowlist in `tests/resource_catalog.rs`.
Prefer `#[doc(hidden)]` or non-`pub` for genuinely internal types.
