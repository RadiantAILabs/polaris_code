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
project, edits code, and runs approved commands through Anthropic or AWS
Bedrock. Model providers, tools, memory, and agent workflows compose as
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

Set your Anthropic API key, then launch Polaris in your project:

```bash
export ANTHROPIC_API_KEY="your-key"
cd your-project
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

### Use AWS Bedrock

Bedrock uses the AWS SDK credential chain. Select it with `--provider bedrock`
or the `POLARIS_CODE_PROVIDER` environment variable:

```bash
export AWS_REGION=us-east-1
polaris --provider bedrock .
```

The SDK also accepts `AWS_PROFILE` or
`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`. Use `--model` to select an
available model. Run `polaris --help` for all command-line options.

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
- **Your provider.** Connect through the Anthropic API or AWS Bedrock.

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
| [Permission modes](docs/permission-modes.md) | Approvals and unattended execution grants. |
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
