# AGENTS.md — polaris_code

Guidance for coding agents working in this workspace. `CLAUDE.md` is a symlink
to this file.

## Workspace

`polaris_code` is a Rust 2024 workspace (MSRV 1.97.0) for the `polaris-code`
terminal coding agent and its supporting crates:

- `crates/polaris_code_core`: agent binary and library, including tools,
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
cargo +1.97.0 build --release -p polaris_code_core
```

`cargo tree -i polaris-ai --locked` should show exactly one `polaris-ai`
package, from crates.io at the version declared by the workspace.

## Code layout

Within `crates/polaris_code_core/src/`:

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
