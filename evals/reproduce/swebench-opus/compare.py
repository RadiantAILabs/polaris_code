"""Compare a polaris-code run with a stock Claude Code run on the same tasks.

Usage:
    python3 evals/reproduce/swebench-opus/compare.py OUT_POLARIS_CODE OUT_CLAUDE_CODE

Prints solved count, total tokens (input + output + cache read + cache
creation), and reported cost for each run, plus the polaris-code / Claude Code
token ratio. Reads only `summary.json` and `episodes.jsonl` from each run.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path


def _polaris_code(run: Path) -> dict[str, float]:
    summary = json.loads((run / "summary.json").read_text(encoding="utf-8"))
    tokens = summary["totals"]["tokens"]
    return {
        "attempted": sum(cell["attempted"] for cell in summary["cells"]),
        "resolved": sum(cell["resolved"] or 0 for cell in summary["cells"]),
        "tokens": tokens["input"] + tokens["output"] + tokens["cache_read"] + tokens["cache_creation"],
        # Bedrock reports no measured cost, so fall back to the priced estimate.
        "cost_usd": summary["totals"]["measured_cost_usd"] or summary["totals"].get("priced_cost_usd") or 0.0,
    }


def _claude_code(run: Path) -> dict[str, float]:
    summary = json.loads((run / "summary.json").read_text(encoding="utf-8"))
    tokens = 0
    with (run / "episodes.jsonl").open(encoding="utf-8") as episodes:
        for line in episodes:
            usage = json.loads(line).get("usage") or {}
            tokens += sum(
                usage.get(field) or 0
                for field in (
                    "input_tokens",
                    "output_tokens",
                    "cache_read_input_tokens",
                    "cache_creation_input_tokens",
                )
            )
    return {
        "attempted": summary["attempted"],
        "resolved": summary["resolved"],
        "tokens": tokens,
        "cost_usd": summary["cost_usd"] or 0.0,
    }


def main(argv: list[str]) -> int:
    if len(argv) != 3:
        print(__doc__.strip(), file=sys.stderr)
        return 2
    rows = {
        "polaris-code": _polaris_code(Path(argv[1])),
        "claude-code": _claude_code(Path(argv[2])),
    }
    print(f"{'run':<14}{'solved':>12}{'tokens':>16}{'cost (USD)':>14}")
    for name, row in rows.items():
        solved = f"{row['resolved']}/{row['attempted']}"
        print(f"{name:<14}{solved:>12}{row['tokens']:>16,}{row['cost_usd']:>14.2f}")
    baseline = rows["claude-code"]["tokens"]
    if baseline:
        print(f"token ratio (polaris-code / claude-code): {rows['polaris-code']['tokens'] / baseline:.1%}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
