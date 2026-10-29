"""The optional gate-plan history records only official grading outcomes."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

from ..plan_history import append_graded_episodes
from ..report import CellReport


class PlanHistoryWriterTests(unittest.TestCase):
    """History rows are append-only projections of real grader verdicts."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.path = Path(holder.name) / "nested" / "plan-history.jsonl"

    def test_appends_one_row_per_graded_episode_with_grader_outcome(self) -> None:
        report = CellReport(
            mode="gates",
            repetition=1,
            run_id="grader-run",
            resolved_ids=frozenset({"task-a"}),
            unresolved_ids=frozenset({"task-b"}),
            empty_patch_ids=frozenset(),
            error_ids=frozenset(),
            graded=True,
            exit_code=0,
        )
        episodes = {
            "task-a": {
                "mode": "gates",
                "repetition": 1,
                "gate_plan_shape_digest": "sha256:shape-a",
                "gate_plan_shape": "Seq(model-propose, apply-patch-intent)",
                "composition_policy": "gates",
                "gate_plan_planner": "polaris-code.gate-planner-model",
                # The writer must not trust this claim over the grader.
                "topology_terminal": "unresolved",
            },
            "task-b": {
                "mode": "gates",
                "repetition": 1,
                "gate_plan_shape_digest": None,
                "gate_plan_shape": None,
                "composition_policy": None,
                "gate_plan_planner": None,
                "topology_terminal": "resolved",
            },
        }

        append_graded_episodes(self.path, report=report, episodes=episodes)
        rows = [json.loads(line) for line in self.path.read_text(encoding="utf-8").splitlines()]

        self.assertEqual([row["task_id"] for row in rows], ["task-a", "task-b"])
        self.assertEqual(rows[0]["shape_digest"], "sha256:shape-a")
        self.assertEqual(rows[0]["shape"], "Seq(model-propose, apply-patch-intent)")
        self.assertEqual(rows[0]["composition_policy"], "gates")
        self.assertEqual(rows[0]["planner"], "polaris-code.gate-planner-model")
        self.assertEqual(rows[0]["outcome"], "resolved")
        self.assertTrue(rows[0]["resolved"])
        self.assertEqual(rows[1]["outcome"], "unresolved")
        self.assertFalse(rows[1]["resolved"])

    def test_ungraded_cell_does_not_write_history(self) -> None:
        report = CellReport(
            mode="gates",
            repetition=1,
            run_id="grader-run",
            resolved_ids=frozenset(),
            unresolved_ids=frozenset(),
            empty_patch_ids=frozenset(),
            error_ids=frozenset(),
            graded=False,
            exit_code=1,
        )

        append_graded_episodes(self.path, report=report, episodes={"task": {}})

        self.assertFalse(self.path.exists())


if __name__ == "__main__":
    unittest.main()
