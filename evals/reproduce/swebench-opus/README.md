# Reproduce the SWE-bench Opus comparison

This kit runs the same 150 public SWE-bench Verified tasks through two arms:

- `polaris-code` with the `gates` composition policy, `careful` topology
  intent, the generic domain, and the single-loop arm; and
- stock Claude Code with the same issue text, checkout-only workspace rule,
  and `medium` effort.

Both arms use Claude Opus 5.5 through AWS Bedrock. This repository contains the
runner and task list, not evaluation outputs; published results are distributed
separately.

## Prerequisites

- Linux x86_64 with Docker and enough disk for SWE-bench images.
- Rust 1.97.0 and the `x86_64-unknown-linux-musl` target. The provided build
  script uses a pinned Docker builder, so the host does not need a musl linker.
- Python 3.11 or newer. Install [requirements.txt](requirements.txt) into a
  dedicated environment; it pins the official SWE-bench grader used by the
  checked-in suite.
- Claude Code on `PATH` for the baseline arm.
- AWS CLI v2 when credentials are supplied by a profile or SSO rather than
  already exported environment variables.
- Bedrock access to Claude Opus 5.5 in a region that serves the `global.`
  inference profile. Set `AWS_REGION` to that region and authenticate through
  the standard AWS chain (environment variables, `AWS_PROFILE`, or SSO).

For example:

```bash
python3 -m venv .venv-swebench
.venv-swebench/bin/python -m pip install -r evals/reproduce/swebench-opus/requirements.txt
export GRADER_PYTHON="$PWD/.venv-swebench/bin/python"
export AWS_REGION=us-west-2
# If applicable:
# export AWS_PROFILE=my-bedrock-profile
# aws sso login --profile "$AWS_PROFILE"
```

The scripts resolve profile or SSO credentials with `aws configure
export-credentials` and pass only the resulting standard AWS environment
variables into disposable task containers. They do not query a machine
metadata endpoint. Each run is paid Bedrock usage; review account limits and
set an appropriate cost ceiling before a full run.

## Build polaris-code

From any directory:

```bash
evals/reproduce/swebench-opus/build.sh
export POLARIS_CODE_BIN="$PWD/target/x86_64-unknown-linux-musl/release/polaris-code"
```

`build.sh` delegates to `scripts/build-portable-binary.sh` and prints the
artifact path. Set `PORTABLE_TARGET_DIR` to place build output elsewhere.

## Smoke test two tasks

Start with two IDs before scheduling the complete set:

```bash
printf '%s\n' 'astropy__astropy-12907,django__django-10554' >/tmp/swebench-opus-smoke.txt

INSTANCES_FILE=/tmp/swebench-opus-smoke.txt \
OUT_DIR=./out/polaris-code-smoke \
API_COST_CEILING_USD=2 \
evals/reproduce/swebench-opus/run-polaris-code.sh

INSTANCES_FILE=/tmp/swebench-opus-smoke.txt \
OUT_DIR=./out/claude-code-smoke \
evals/reproduce/swebench-opus/run-claude-code.sh
```

A relative `OUT_DIR` is taken from the directory you run the script from; the
default is `out/<name>` at the repository root. Run directories are immutable
and must not already contain a prior run.

## Run all 150 tasks

The default `INSTANCES_FILE` is this directory's [instances.txt](instances.txt).
The polaris-code script requires `POLARIS_CODE_BIN` and `GRADER_PYTHON`; the
Claude Code script requires `GRADER_PYTHON` and defaults `CLAUDE_BIN` to
`claude` on `PATH`.

```bash
export API_COST_CEILING_USD=150  # safety ceiling, not an expected bill
evals/reproduce/swebench-opus/run-polaris-code.sh
evals/reproduce/swebench-opus/run-claude-code.sh
```

Defaults may be overridden with:

- `OUT_DIR` (default `out/polaris-code` or `out/claude-code` at the repo root; a relative path is taken from the current directory)
- `MAX_WORKERS` (grader workers, default `2`)
- `EPISODE_WORKERS` (concurrent agent tasks, default `1`)
- `INSTANCES_FILE`
- `API_COST_CEILING_USD` for polaris-code (default: one US dollar per selected
  task as a scheduling safety ceiling)
- `MODEL` (`aws.bedrock/global.anthropic.claude-opus-5-5` for polaris-code;
  `global.anthropic.claude-opus-5-5` for Claude Code)
- `ARM` for polaris-code (default `loop`)
- `CLAUDE_BIN` for the baseline

The composition-axis defaults intentionally provide graph-search mode `both`,
enable topology work, and pass a 16,384-token output limit to each model
request. The script leaves those defaults in one authoritative place, the
harness, and the generated `manifest.json` records them.

## Read the outputs

Each polaris-code run writes `manifest.json`, `episodes.jsonl`, `summary.json`,
official-format predictions, grader output, and—because this reproduction keeps
transcripts—per-task logs. In `summary.json`:

- `cells[0].resolved` is the official solved-task count for the `gates` cell;
- `totals.tokens` contains input, output, cache-read, and cache-creation token
  totals; and
- `totals.measured_cost_usd` is the summed run-reported estimate when every
  episode was priced.

The Claude Code baseline writes the official solved count as `resolved` and
its aggregate reported cost as `cost_usd` in `summary.json`. Per-task token
usage is in each line's `usage` object in `episodes.jsonl`; sum the corresponding
input, output, cache-read, and cache-creation fields to compare token use with
the polaris-code tiers. `manifest.json` records the model, effort, task IDs,
timeouts, and Claude Code version used.

To print both runs side by side, with the token ratio:

```bash
python3 evals/reproduce/swebench-opus/compare.py out/polaris-code out/claude-code
```

Official grading artifacts live below each run's `grader/` directory. Keep the
two run directories together when comparing arms so the task set and manifests
remain auditable. Evaluation output can contain upstream issue text and model
transcripts; store and publish it separately from this source repository.
