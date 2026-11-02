<p align="center">
  <img src="assets/polaris-logo.svg#gh-light-mode-only" width="560" alt="Polaris Code">
  <img src="assets/polaris-logo-dark.svg#gh-dark-mode-only" width="560" alt="Polaris Code">
</p>

<p align="center">
  <strong>A fast, token-efficient coding agent. Built to compose.</strong>
</p>

<p align="center">
  <a href="https://radiantlabs.dev"><img src="https://img.shields.io/badge/website-radiantlabs.dev-000000?labelColor=000000&amp;style=flat-square" alt="Website: radiantlabs.dev"></a>
  <a href="https://github.com/RadiantAILabs/polaris_code/tags"><img src="https://img.shields.io/github/v/tag/RadiantAILabs/polaris_code?label=release&amp;color=000000&amp;labelColor=000000&amp;style=flat-square" alt="Latest release tag"></a>
  <a href="https://crates.io/crates/polaris_code"><img src="https://img.shields.io/crates/v/polaris_code?color=000000&amp;labelColor=000000&amp;style=flat-square" alt="Crates.io version"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-000000?labelColor=000000&amp;style=flat-square" alt="License: Apache 2.0"></a>
</p>

<p align="center">
  <a href="https://radiantlabs.dev"><strong>Website</strong></a> ·
  <a href="#get-started"><strong>Get started</strong></a> ·
  <a href="https://github.com/RadiantAILabs/polaris_code/releases"><strong>Releases</strong></a> ·
  <a href="#documentation"><strong>Documentation</strong></a> ·
  <a href="#benchmark"><strong>Benchmark</strong></a>
</p>

Polaris Code is a terminal coding agent written in Rust. It reads your
project, edits code, and runs approved commands through Anthropic, OpenAI,
any OpenAI-compatible server, or AWS Bedrock. Model providers, tools, memory, and agent workflows compose as
plugins, and every decision and tool call is recorded in the session graph.

In a comparison on 150 SWE-bench Verified tasks, Polaris Code solved
136 tasks using **16% of the tokens** and **less than half the estimated
cost** of Claude Code with the same model. See the
[benchmark and reproduction guide](#benchmark) for the measurements and scope.

## Get started

### Install

```bash
brew install radiantailabs/tap/polaris-code
```

Prebuilt Homebrew bottles are available for Apple Silicon on macOS Sequoia
and newer, and Linux x86-64. Other configurations build from source.
The formula is named `polaris-code`; the installed command is `polaris`.

Or install with Cargo using Rust 1.97.0 or newer:

```bash
cargo install polaris_code --locked
```

### Start a session

Save a key once, then launch Polaris in your project. `polaris login` asks
which provider you use, reads its key with the input hidden, stores it where
only you can read it, and makes that provider your default:

```bash
polaris login            # Anthropic, OpenAI or AWS Bedrock
polaris login status     # which providers have credentials, and from where
cd your-project
polaris .
```

Or export a key instead. An environment variable, or a `.env` file in the
folder you launch from, wins over a saved key:

```bash
export ANTHROPIC_API_KEY="your-key"
polaris .
```

### Run a single task

Use `-p` / `--print` for one unattended turn:

```bash
polaris -p "Summarize this workspace" --output-format json
```

Interactive sessions ask before edits and commands. Unattended runs require
explicit per-run grants to write files or execute commands. See
[permission modes](docs/permission-modes.md) for the available settings.

### Choose a provider

Select a provider with `--provider anthropic|openai|bedrock` or the
`POLARIS_CODE_PROVIDER` environment variable. OpenAI uses `OPENAI_API_KEY`, and
`OPENAI_BASE_URL` points it at an OpenAI-compatible server (Ollama, LM Studio,
vLLM, a gateway). Bedrock uses the AWS SDK credential chain:

```bash
export AWS_REGION=us-east-1
polaris --provider bedrock .
```

The SDK also accepts `AWS_PROFILE` or
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`. Before the terminal opens, Polaris
checks the provider's credentials and, if something is missing, says what to
do.

`polaris --list-models` shows the models each provider offers, their prices,
and whether the credentials are present; `--model sonnet` picks the newest
Sonnet. Run `polaris --help` for all command-line options, and see
[providers](docs/providers.md) for OpenAI-compatible servers, where credentials
are stored, and the startup check.

### Keep separate setups

Set `POLARIS_CONFIG_DIR` to keep independent user setups:

```bash
POLARIS_CONFIG_DIR=~/.polaris-work polaris
```

This directory holds `config.toml` and its profiles, `credentials.toml`,
`permissions.toml`, and `mcp.json`. When unset or empty, it falls back to
`$XDG_CONFIG_HOME/polaris`, then `~/.config/polaris`. Project configuration and
session state stay in the workspace. `/status` shows the resolved directory and
whether `POLARIS_CONFIG_DIR` selected it. See
[startup configuration](docs/configuration.md).

### Set defaults

Put defaults in `config.toml` (yours) or `polaris.toml` (the project's). The
accepted keys are `provider`, `model`, `models`, `region`, `permission_mode`
(`confirm`, `auto-edit`, `auto`, or `plan`), `permission_ceiling`,
`max_cost_usd`, `max_tokens`, `max_turns`, and `max_output_tokens`. Flags and
environment variables win over both files.

Two options are flags or environment variables only: `--notify` /
`POLARIS_CODE_NOTIFY` (`off`, `approval`, or `all`; when the terminal bell and
window title ask for attention, `approval` by default) and `--review-model` /
`POLARIS_CODE_REVIEW_MODEL` (the model for automatic review).

A repository cannot loosen your permissions or start its own programs until you
trust it. Polaris ignores a `polaris.toml` permission setting that is more
permissive than yours, and does not start servers from a project `.mcp.json`.
Review them, then use `/permissions trust-project` or `/mcp trust`. Trust is
saved per project and ends when the reviewed file changes. See
[startup configuration](docs/configuration.md#project-files-and-trust) and
[MCP servers](docs/mcp.md#project-servers-and-trust).

## Why Polaris Code

- **Composable by design.** Replace model providers, tools, memory, or graph
  steps through declared plugin contracts.
- **Token efficient.** Plan the task and project the context needed for each
  step. The [benchmark](#benchmark) records a comparison you can reproduce.
- **Native Rust.** One executable, with no Node runtime, daemon, or telemetry.
- **Inspectable sessions.** Trace decisions, tool calls, evidence, and
  outcomes. Review changes and rewind a session.
- **Explicit permissions.** Interactive edits and commands ask first;
  unattended execution uses explicit grants.
- **Your provider.** Connect through the Anthropic API, the OpenAI API, any
  OpenAI-compatible server, or AWS Bedrock.

## Benchmark

150 SWE-bench Verified tasks, Claude Opus 5.5 on AWS Bedrock, one run per task
for each agent:

| | Polaris Code | Claude Code |
| --- | ---: | ---: |
| Solved | 136 / 150 | 135 / 150 |
| Total tokens | 8,179,965 | 50,052,324 |
| Cost (USD, estimated) | 14.20 | 32.96 |
| Typical task time | ~30 s | ~110 s |

Tokens count input, output, cache read, and cache write. Bedrock does not return
a measured cost, so both costs are list-price estimates. These measurements
describe this run and task sample. The full results are published separately.
This repository contains the code and pinned inputs to re-run the comparison:
see the [reproduction guide](evals/reproduce/swebench-opus/README.md).

## How it works

The interactive terminal and headless command drive the same Polaris session
and reflective ReAct loop. Tools read and edit files, run approved commands,
search the workspace, query a multi-language code index, and store memories.
The topology catalog selects and composes bounded task graphs; the session
graph records their decisions and outcomes.

Agent behavior is a graph of typed Rust systems. Replace a step or rewire a
workflow to try another approach. The [design philosophy](docs/philosophy.md)
explains the architecture.

## Documentation

| Guide | Covers |
| --- | --- |
| [Permission modes](docs/permission-modes.md) | Approvals, rules, automatic review, and unattended execution grants. |
| [Providers](docs/providers.md) | Provider selection, `polaris login`, and OpenAI-compatible servers. |
| [Configuration](docs/configuration.md) | Config files, profiles, `POLARIS_CONFIG_DIR`, and `/status`. |
| [MCP servers](docs/mcp.md) | Connecting tools from Model Context Protocol servers. |
| [Web access](docs/web-access.md) | The `web_fetch` tool and its address limits. |
| [Context compaction](docs/context-compaction.md) | Summaries, `/compact`, and what is kept. |
| [Project instructions](docs/project-instructions.md) | How project guidance is discovered and applied. |
| [Plugin integration](docs/reference/guide.md) | Composing and extending the existing capabilities. |
| [Embedding](crates/polaris_code/src/docs/embedding.md) | Using the coding agent in a Rust application. |
| [Topology selection](docs/topology-selection.md) | Graph admission, selection, and recorded explanations. |
| [Publishing](docs/publishing.md) | Registry packages, release archives, and Homebrew. |
| [Benchmark reproduction](evals/reproduce/swebench-opus/README.md) | Pinned inputs and commands for the comparison above. |

Browse [`docs/`](docs/) for the full documentation.

## Use as a library

```toml
[dependencies]
polaris_code = "0.1.1"
```

Compose model, tool, and I/O plugins on a Polaris `Server`, create a session,
then call `polaris_code::run::drive_turn`. The
[embedding guide](crates/polaris_code/src/docs/embedding.md) includes a complete
example and the permission and persistence contracts your application supplies.

## Development

Build with the pinned Rust 1.97.0 toolchain:

```bash
git clone https://github.com/RadiantAILabs/polaris_code.git
cd polaris_code
cargo +1.97.0 build --release --locked -p polaris_code
./target/release/polaris .
```

Run the workspace checks:

```bash
cargo +1.97.0 check --workspace --locked
cargo +1.97.0 test --workspace --locked
cargo +1.97.0 fmt --all -- --check
cargo +1.97.0 clippy --workspace --all-targets --all-features --locked -- -D warnings
```

GitHub Actions checks the workspace on Linux and macOS and builds release
archives for Linux x86-64 and Apple Silicon macOS. Matching `v*` tags in the
public repository stage a draft release with those same archives, SHA-256
checksums, and a Homebrew formula. See
[CI and binary releases](docs/publishing.md#ci-and-binary-releases) for
platform requirements and the tap handoff.

| Path | Purpose |
| --- | --- |
| `crates/polaris_code` | CLI, coding agent, tools, and topology host. |
| `crates/polaris_cosmos` | Composition contracts, topology admission, and evidence. |
| `crates/polaris_ontrace` | Temporal knowledge graph backed by Oxigraph. |
| `crates/polaris-tui` | Reusable terminal shell and panels. |
| `crates/polaris-tui-examples` | Example TUI binaries. |
| `evals/` | Evaluation harness, dataset packs, and external-agent baselines. |

All crates inherit the single crates.io `polaris-ai` dependency from the root
`Cargo.toml`.

## Contributing

Open an [issue](https://github.com/RadiantAILabs/polaris_code/issues) for a bug
report or feature proposal, or send a pull request with a focused change.
For architecture changes, start with the [design philosophy](docs/philosophy.md)
and [plugin integration guide](docs/reference/guide.md). Include the relevant
workspace checks when submitting code.

## License

Apache License 2.0. See [LICENSE](LICENSE).

Built by [Radiant Labs](https://radiantlabs.dev).
