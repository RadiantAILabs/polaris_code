"""Tests the results-table renderer against hand-built outcomes.

No model, no binary: every `FixtureRun` here is constructed directly.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from .. import checks, expect as expect_mod, report as report_mod, runner as runner_mod

TASKS_DIR = Path(__file__).resolve().parents[3] / "evals" / "tasks"


def _run(expect: expect_mod.FixtureExpect, *, report: dict | None, skipped: bool = False) -> runner_mod.FixtureRun:
    return runner_mod.FixtureRun(
        expect=expect,
        workspace_dir=Path("."),
        argv=[],
        returncode=0 if report else None,
        stdout="",
        stderr="",
        report=report,
        tree_unchanged=True,
        skipped=skipped,
        skip_reason="conversational route; pass --include-conversational to run it" if skipped else None,
    )


class RenderTest(unittest.TestCase):
    def test_a_passing_row_names_the_registration_recipe_and_blocks(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "py-oneline-fix")
        report = {
            "status": "completed",
            "result": "ok",
            "topology": {
                "registration_id": "code.direct-edit",
                "composition": {
                    "recipe": "polaris-code.recipe.direct-verified@1.0.0",
                    "blocks": ["cosmos.verified-step@1.0.0", "cosmos.resolve-residuals@1.0.0"],
                    "digest": {"algorithm": "sha256", "hex": "ab"},
                },
            },
        }
        run = _run(loaded, report=report)
        outcome = checks.check_outcome(loaded.outcome, Path("/nonexistent"), report)
        plan = checks.check_plan(loaded.plan, report)
        table = report_mod.render(
            [report_mod.FixtureOutcome(run=run, outcome=outcome, plan=plan)],
            composition_policy="deterministic",
        )
        self.assertIn("code.direct-edit", table)
        self.assertIn("cosmos.verified-step", table)
        self.assertIn("algorithm=sha256", table)

    def test_a_skipped_row_never_claims_pass_or_fail(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-repair-loop")
        run = _run(loaded, report=None, skipped=True)
        outcome = checks.CheckResult(True, "skipped")
        plan = checks.CheckResult(True, "skipped")
        table = report_mod.render(
            [report_mod.FixtureOutcome(run=run, outcome=outcome, plan=plan)],
            composition_policy="deterministic",
        )
        self.assertIn("skipped", table)
        self.assertNotIn("| pass |", table)

    def test_a_failure_detail_with_a_pipe_does_not_break_the_table(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "py-oneline-fix")
        run = _run(loaded, report=None)
        outcome = checks.CheckResult(False, "cargo test exited 1: line one | line two")
        plan = checks.CheckResult(True, "n/a")
        table = report_mod.render(
            [report_mod.FixtureOutcome(run=run, outcome=outcome, plan=plan)],
            composition_policy="deterministic",
        )
        # Six cells means seven unescaped `|` delimiters (one on each side plus
        # five separators); the embedded pipe must not add an eighth.
        data_row = next(line for line in table.splitlines() if line.startswith("| `py-oneline-fix`"))
        self.assertEqual(data_row.count("|") - data_row.count("\\|"), 7)


if __name__ == "__main__":
    unittest.main()
