# polaris-code

**A blazingly fast, token-efficient coding harness. Same solve rate as Claude Code, 6x fewer tokens.**

Written in Rust. One static binary. Every decision,
tool call and token is recorded where you can read it.

| 150 SWE-bench Verified tasks, Claude Opus 5.5 | Solved | Tokens | Cost | Typical task time |
| --- | ---: | ---: | ---: | ---: |
| **polaris-code** | **136** | **8.2M** | **$14.20** | **~30 s** |
| Claude Code | 135 | 50.1M | $32.96 | ~110 s |

Same model, same tasks, same machine. polaris-code used **16% of the tokens**
and **less than half the cost**, and finished a typical task in about 30
seconds where Claude Code took about 110. Every number here can be re-run from
this repo: see [Benchmark](#benchmark).

## Why polaris-code

- **Composable architecture. Everything is a plugin.** Model providers, tools,
  memory, I/O, and the agent loop compose through explicit contracts. Swap
  capabilities and experiment with workflows without redesigning the rest of
  the agent.
- **Token efficient.** It plans the task first, then runs only the steps the
  task needs, so the model reads far less context on every turn.
- **Blazingly fast.** Native Rust. No Node runtime, no Electron, nothing to boot.
- **Lightweight.** One static binary. No daemon, no cloud service, no telemetry.
- **Fully inspectable.** Each session is a graph of decisions, tool calls,
  evidence and outcomes. You can see why it did what it did, and rewind it.
- **Safe by default.** Edits and commands ask first. Unattended runs can write
  or execute only with an explicit grant.
- **Bring your model.** Anthropic API or AWS Bedrock.

## Workspace

| Path | Purpose |
| --- | --- |
| `crates/polaris_code` | The `polaris-code` binary, agent loop, tools, terminal commands, and coding-specific topology host. |
| `crates/polaris_cosmos` | Domain-neutral composition contracts, admission rules, evidence types, and gate blocks. |
| `crates/polaris_ontrace` | Temporal knowledge-graph support backed by Oxigraph. |
| `crates/polaris-tui` | Reusable ratatui shell and panel registry. |
| `crates/polaris-tui-examples` | Small binaries demonstrating the TUI crate. |
| `evals/` | Dataset-neutral evaluation harness, SWE-bench packs, and external-agent baselines. |

Every crate uses the single `polaris-ai` dependency (from crates.io) declared in
the root `Cargo.toml`, so shared framework types resolve to one package instance
throughout the workspace.

## Build

Rust 1.97.0 is required.

```bash
cargo +1.97.0 build --release -p polaris_code
```

The resulting binary is `target/release/polaris-code`. For a complete
development check, run:

```bash
cargo +1.97.0 check --workspace
cargo +1.97.0 test --workspace
cargo +1.97.0 fmt -- --check
cargo +1.97.0 clippy --all-targets --all-features -- -D warnings
```

## Quick start

The default provider is Anthropic:

```bash
export ANTHROPIC_API_KEY=your-key
target/release/polaris-code .
```

Pass `-p`/`--print` for one unattended turn:

```bash
target/release/polaris-code -p "summarize this workspace" --output-format json
```

AWS Bedrock uses the AWS SDK credential chain. Select it with either the flag
or the environment variable:

```bash
export POLARIS_CODE_PROVIDER=bedrock
export AWS_REGION=us-east-1
target/release/polaris-code .
```

The SDK also accepts `AWS_PROFILE` or
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`. Use `--model` to select an
available model, and run `polaris-code --help` for all provider, model,
permission, output, and topology options.

Interactive sessions default to confirmation for edits and commands.
Unattended runs are fail-closed: writing or executing commands requires the
corresponding explicit per-run grant. See
[`docs/permission-modes.md`](docs/permission-modes.md).

## Use as a library

The `polaris_code` package exposes the coding agent as a Rust library; the
executable remains `polaris-code`.

```toml
[dependencies]
polaris_code = "0.1.0"
```

Compose model, tool, and I/O plugins on a Polaris `Server`, create a session,
then call `polaris_code::run::drive_turn`. The
[embedding guide](crates/polaris_code/src/docs/embedding.md) includes the full
dependency list, a complete example, and the permission and persistence
contracts an embedding application supplies.

## How it works

Both front ends drive the same Polaris session and reflective ReAct loop.
Tools can read and edit files, run approved commands, search the tree, query a
multi-language code index, and store content-hash-invalidated memories. The
topology catalog can admit, select, compose, and explain bounded task graphs;
the session graph records decisions, tool calls, evidence, and outcomes.

Agent behavior is a graph of typed Rust systems. Replace a step or rewire a
workflow to try a different approach to the same task. The
[design philosophy](docs/philosophy.md) explains the architecture, and the
[integration guide](docs/reference/guide.md) shows how to compose and extend
the existing plugins.

The detailed documentation under [`docs/`](docs/) covers topology selection,
gate planning, the evaluation harness, permissions, session rewind, usage
accounting, and the TUI integration.

## Benchmark

150 SWE-bench Verified tasks, Claude Opus 5.5 on AWS Bedrock, one run per task
for each agent:

| | polaris-code | Claude Code |
| --- | ---: | ---: |
| Solved | 136 / 150 | 135 / 150 |
| Total tokens | 8,179,965 | 50,052,324 |
| Cost (USD) | 14.20 | 32.96 |
| Typical task time | ~30 s | ~110 s |

Tokens count input, output, cache read and cache write. Bedrock does not return
a measured cost, so both costs are list-price estimates. The full results are
published separately. This repository contains the code and pinned inputs to
re-run the comparison yourself: see the
[`swebench-opus` reproduction guide](evals/reproduce/swebench-opus/README.md).

## License

Licensed under the [Apache License 2.0](LICENSE).
