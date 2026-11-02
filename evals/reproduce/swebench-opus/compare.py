"""Compare a polaris-code run with a stock Claude Code run on the same tasks.

Usage:
    python3 evals/reproduce/swebench-opus/compare.py OUT_POLARIS_CODE OUT_CLAUDE_CODE

Each argument is one run directory, or several separated by commas when a run
was split into chunks (for example to fit SWE-bench images on disk). Chunks
must cover disjoint tasks.

Prints solved count, total tokens (input + output + cache read + cache
creation), reported cost, and median task time for each run, plus the
polaris-code / Claude Code token ratio. Reads only `summary.json` and
`episodes.jsonl` from each run.
"""

from __future__ import annotations

import json
import statistics
import sys
from pathlib import Path


def _polaris_code(run: Path) -> dict:
    summary = json.loads((run / "summary.json").read_text(encoding="utf-8"))
    tokens = summary["totals"]["tokens"]
    with (run / "episodes.jsonl").open(encoding="utf-8") as episodes:
        seconds = [json.loads(line).get("inference_seconds") or 0.0 for line in episodes]
    return {
        "attempted": sum(cell["attempted"] for cell in summary["cells"]),
        "resolved": sum(cell["resolved"] or 0 for cell in summary["cells"]),
        "tokens": tokens["input"] + tokens["output"] + tokens["cache_read"] + tokens["cache_creation"],
        # Bedrock reports no measured cost, so fall back to the priced estimate.
        "cost_usd": summary["totals"]["measured_cost_usd"] or summary["totals"].get("priced_cost_usd") or 0.0,
        "seconds": seconds,
    }


def _claude_code(run: Path) -> dict:
    summary = json.loads((run / "summary.json").read_text(encoding="utf-8"))
    tokens = 0
    seconds = []
    with (run / "episodes.jsonl").open(encoding="utf-8") as episodes:
        for line in episodes:
            episode = json.loads(line)
            usage = episode.get("usage") or {}
            tokens += sum(
                usage.get(field) or 0
                for field in (
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                )
            )
            seconds.append(episode.get("seconds") or 0.0)
    return {
        "attempted": summary["attempted"],
        "resolved": summary["resolved"],
        "tokens": tokens,
        "cost_usd": summary["cost_usd"] or 0.0,
        "seconds": seconds,
    }


def _combined(runs: str, read) -> dict:
    parts = [read(Path(run)) for run in runs.split(",") if run]
    return {
        "attempted": sum(part["attempted"] for part in parts),
        "resolved": sum(part["resolved"] for part in parts),
        "tokens": sum(part["tokens"] for part in parts),
        "cost_usd": sum(part["cost_usd"] for part in parts),
        "seconds": [second for part in parts for second in part["seconds"]],
    }


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    rows = {
        "polaris-code": _combined(argv[1], _polaris_code),
        "claude-code": _combined(argv[2], _claude_code),
    }
    print(f"{'run':<14}{'solved':>12}{'tokens':>16}{'cost (USD)':>14}{'median s':>10}")
    for name, row in rows.items():
        solved = f"{row['resolved']}/{row['attempted']}"
        median = statistics.median(row["seconds"]) if row["seconds"] else 0.0
        print(f"{name:<14}{solved:>12}{row['tokens']:>16,}{row['cost_usd']:>14.2f}{median:>10.1f}")
    baseline = rows["claude-code"]["tokens"]
    if baseline:
        print(f"token ratio (polaris-code / claude-code): {rows['polaris-code']['tokens'] / baseline:.1%}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
