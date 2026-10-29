# AGENTS.md — polaris_code

Guidance for coding agents working in this workspace. `CLAUDE.md` is a symlink
to this file.

## Workspace

`polaris_code` is a Rust 2024 workspace (MSRV 1.97.0) for the `polaris`
terminal coding agent and its supporting crates:

- `crates/polaris_code`: agent binary and library, including tools,
  terminal front ends, session handling, and the coding topology host.
- `crates/polaris_cosmos`: composition contracts, topology admission,
  selection, evidence, and reusable gate blocks.
- `crates/polaris_ontrace`: Oxigraph-backed temporal knowledge graph plugin.
- `crates/polaris-tui`: reusable ratatui shell and panels.
- `crates/polaris-tui-examples`: example TUI binaries.
- `evals/`: Python evaluation core, dataset packs, fixtures, and external-agent
  adapters.
- `scripts/`: build and evaluation helpers.

The workspace takes Polaris (`polaris-ai`) from crates.io through the single
dependency in the root `Cargo.toml`. All workspace crates must inherit that
dependency so framework types unify. Do not add another `polaris-ai` source or
change its version as part of unrelated work.

## Design philosophy

Read [`docs/philosophy.md`](docs/philosophy.md) and
[`docs/taxonomy.md`](docs/taxonomy.md) when designing or reviewing a change.
The shared principles come from
[Polaris Core Philosophy](https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md).

- **Everything is a plugin.** Deliver capabilities through small,
  self-contained plugins with declared requirements and narrow public
  resources/APIs. Compose through those contracts, never another component's
  internals. Helpers and systems live inside capabilities; they do not each
  need a plugin wrapper.
- **Compose, replace, and reuse.** Extend registries and compose existing
  building blocks before adding another abstraction. A capability should be
  replaceable or omittable when the assembly's declared requirements remain
  satisfied. Document its contracts using [`docs/reference/`](docs/reference/).
- **Separate state from behavior.** Put state in scoped typed resources and
  behavior in systems with explicit inputs. Use `Res<T>` for reads,
  `ResMut<T>` for mutable local state, and `Out<T>` for results passed between
  systems; avoid a catch-all struct of optional intermediate values.
- **The graph is the agent.** Compose systems and edges to change behavior.
  Contribute topologies through packs and the shared catalog rather than
  special cases in the host's selection loop.
- **Keep the composition root thin.** `main.rs` chooses and wires plugins,
  configuration, and front ends. Tool behavior belongs in capabilities; agent
  decisions belong in systems and policy seams. Tools must not depend on the
  agent's `LoopState`.
- **Make experiments observable.** Preserve the policy-read and
  trajectory-write seams and the topology evidence contracts. Components
  should be testable in isolation so a design hypothesis can be tried and
  assessed without redesigning unrelated parts.

## Build and test

Use the pinned toolchain for every Rust command:

```bash
cargo +1.97.0 check --workspace
cargo +1.97.0 test --workspace
cargo +1.97.0 fmt -- --check
cargo +1.97.0 clippy --all-targets --all-features -- -D warnings
```

Build the release agent with:

```bash
cargo +1.97.0 build --release -p polaris_code
```

`cargo tree -i polaris-ai --locked` should show exactly one `polaris-ai`
package, from crates.io at the version declared by the workspace.

## Registry packages

`polaris_code` is the public application library and ships the `polaris`
binary. Its supporting crates remain independently reusable registry packages:
`polaris_cosmos`, `polaris_ontrace`, and `polaris-tui`. Keep the examples crate
unpublished and keep version requirements on workspace path dependencies.

See [`docs/publishing.md`](docs/publishing.md) for archive verification and the
release process. Publication checks must build the packaged sources, not only
the workspace checkout.

## Code layout

Within `crates/polaris_code/src/`:

- `main.rs` assembles the server, providers, front end, tools, and agent.
- `run.rs` is the shared interactive and headless turn driver.
- `agents/` owns the ReAct loop, policy snapshots, context projection, project
  grounding, and trajectory recording.
- `tools/` owns sandboxed file, shell, code-graph, concept, and memory tools.
- `cosmos/` adapts `polaris_cosmos` contracts to coding tasks and hosts
  topology selection, composition, execution, and evidence.
- `terminal/` owns TUI commands, transcript projection, session navigation,
  attachments, rewind, and change review.
- `exec/` owns the non-interactive provider and output formats.

Add tools as self-contained modules under `tools/` and register them through a
plugin. Add agent decision seams under `agents/`. Add coding topology graphs
under `cosmos/graphs.rs`, register them in `cosmos/topology.rs`, and update the
type registry and availability probe for every new graph-boundary type.

The topology catalog is sealed during startup. A complete registration with an
unexplained boundary is an assembly error; a capability absent from the host
probe is a runtime exclusion. Listing surfaces derive from the sealed catalog,
so new registrations should not need hand-maintained listing entries.

## Conventions

- Document public items; the workspace warns on missing documentation.
- Use `tracing` in libraries and plugins. The TUI owns stdout.
- Keep tools confined to the configured working directory.
- Preserve the distinction between canonical session state and UI projections.
- Keep topology explanations as projections of recorded decisions; do not
  re-rank or recompute them while rendering.
- Keep dataset-neutral evaluation logic in `evals/core/` and dataset-specific
  policy, environments, graders, and suites in `evals/packs/<name>/`.
- Do not check evaluation outputs, transcripts, logs, credentials, or local
  environment files into the repository.
- Build custom TUI widgets against the `polaris_tui::ratatui` re-export to keep
  the UI stack on one version.

The composition root has three intentional stdout surfaces: the headless result
emitter, the post-TUI usage summary, and `--list-topologies`. Their bytes are
rendered by `exec::report`. Startup diagnostics may use stderr before the TUI
installs its tracing subscriber.

## Permissions and safety

Interactive sessions begin in `confirm` mode. `--print` is unattended and
fail-closed: edits and shell commands require explicit grants for that run.
Keep new mutating tools aligned with the permission policy, edit journal,
index-refresh path, and session review/rewind behavior.

Do not weaken path containment, symlink checks, command restrictions, secret
redaction, or evaluation spend guards without updating their tests and public
documentation.
