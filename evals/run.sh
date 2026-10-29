#!/usr/bin/env bash
# Runs the dynamic-topology fixture suite: for each fixture under
# evals/tasks/, drives the shipped polaris-code binary through the headless
# entry point its expect.toml names, checks the outcome and the chosen plan,
# and writes target/eval-reports/dynamic-topology/<timestamp>.md by default.
# See evals/README.md.
#
# Usage:
#   evals/run.sh [--binary PATH] [--composition-policy deterministic|model] \
#                [--include-conversational] [--keep-tmp] [fixture-name ...]
#
# The binary is located the same way evals/dynamic_topology/cli.py's own
# default does: --binary wins, then $POLARIS_CODE_BIN, then target/debug or
# target/release under this repository. Building one:
#   cargo build --bin polaris-code
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

exec python3 -m evals.dynamic_topology run "$@"
