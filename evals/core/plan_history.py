"""Append officially graded gate-plan outcomes to optional JSONL history."""

from __future__ import annotations

import json
from pathlib import Path
from typing import Mapping

from .report import CellReport, RESOLVED

#: Optional environment default for the cross-run gate-plan history file.
PLAN_HISTORY_VAR = "POLARIS_CODE_PLAN_HISTORY"


def append_graded_episodes(
    path: Path,
    *,
    report: CellReport,
    episodes: Mapping[str, Mapping[str, object]],
) -> None:
    """Append one real-grader outcome for every episode in a graded cell.

    ``CellReport.disposition`` is the sole outcome source. In particular, an
    agent's own terminal answer is never consulted. Episodes without an
    admitted gate plan still receive a record; their null shape fields let the
    history faithfully represent the graded episode while the Rust reader
    simply excludes them from per-shape summaries.
    """
    if not report.graded:
        return

    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as history:
        for task_id, episode in episodes.items():
            disposition = report.disposition(task_id)
            record = {
                "task_id": task_id,
                "mode": episode.get("mode"),
                "repetition": episode.get("repetition"),
                "shape_digest": episode.get("gate_plan_shape_digest"),
                "shape": episode.get("gate_plan_shape"),
                "composition_policy": episode.get("composition_policy"),
                "planner": episode.get("gate_plan_planner"),
                "outcome": disposition,
                "resolved": disposition == RESOLVED,
            }
            history.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")
        history.flush()
