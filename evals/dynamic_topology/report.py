"""Renders one run of the dynamic-topology fixture suite as a markdown table."""

from __future__ import annotations

import dataclasses
import datetime
from pathlib import Path
from typing import Any

from .checks import CheckResult
from .runner import FixtureRun


@dataclasses.dataclass(frozen=True)
class FixtureOutcome:
    """One fixture's run plus its two check verdicts, ready to render."""

    run: FixtureRun
    outcome: CheckResult
    plan: CheckResult


def _compact_plan(report: dict[str, Any] | None) -> str:
    topology = (report or {}).get("topology")
    if topology is None:
        return "conversational (no topology decision)"
    composition = topology.get("composition") or {}
    blocks = "+".join(composition.get("blocks") or ()) or "—"
    return f"{topology.get('registration_id')} / {composition.get('recipe')} / {blocks}"


def _plan_digest(report: dict[str, Any] | None) -> str:
    topology = (report or {}).get("topology")
    if topology is None:
        return "—"
    composition = topology.get("composition") or {}
    digest = composition.get("digest")
    if digest is None:
        return "—"
    if isinstance(digest, dict):
        # `ContentDigest`'s own serialization shape is Cosmos's, not this
        # package's; render whatever fields it carries rather than assuming one.
        return ",".join(f"{key}={value}" for key, value in sorted(digest.items()))
    return str(digest)


def _verdict(result: CheckResult) -> str:
    if result.ok:
        return "pass"
    # A failure detail can itself contain `|` (a command's own stdout tail,
    # joined with " | ") or a newline, either of which would break this row
    # out of its markdown table cell.
    detail = result.detail.replace("|", "\\|").replace("\n", " ")
    return f"fail — {detail}"


def render(outcomes: list[FixtureOutcome], *, composition_policy: str) -> str:
    """Renders the results table. Does not write anything."""
    lines = [
        "# Dynamic-topology fixture run",
        "",
        f"Composition policy: `{composition_policy}`.",
        "",
        "| task | shape | plan digest | compact plan | outcome | plan properties |",
        "|---|---|---|---|---|---|",
    ]
    for item in outcomes:
        run = item.run
        if run.skipped:
            reason = (run.skip_reason or "").replace("|", "\\|").replace("\n", " ")
            lines.append(
                f"| `{run.expect.name}` | {run.expect.shape} | — | skipped | "
                f"skipped — {reason} | skipped |"
            )
            continue
        lines.append(
            f"| `{run.expect.name}` | {run.expect.shape} | {_plan_digest(run.report)} | "
            f"{_compact_plan(run.report)} | {_verdict(item.outcome)} | {_verdict(item.plan)} |"
        )
    lines.append("")
    return "\n".join(lines)


def write(outcomes: list[FixtureOutcome], *, composition_policy: str, results_dir: Path) -> Path:
    """Writes the rendered table to ``<results_dir>/<timestamp>.md`` and returns the path."""
    results_dir.mkdir(parents=True, exist_ok=True)
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    path = results_dir / f"{timestamp}.md"
    path.write_text(render(outcomes, composition_policy=composition_policy))
    return path
