"""Deterministic tests for the fixture checkers themselves.

None of these tests start ``polaris-code`` or call a model. A ``command``
outcome check is run for real (``cargo test`` / ``python3 -m unittest``), but
only against this repository's own checked-in ``workspace/`` (unsolved) and
``solved/`` copies, which proves each checker in both directions. An
``answer_contains``/plan check is exercised
against small, hand-built report dictionaries shaped like
``crates/polaris_code_core/src/exec/report.rs``'s ``RunReport`` JSON — this
package has no Rust FFI, so the shape is asserted by field name rather than
imported, and a change to that struct's serialized field names would need this
file updated to match, exactly as it would need every downstream JSON consumer
updated.
"""

from __future__ import annotations

import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any

from .. import checks, expect as expect_mod

TASKS_DIR = Path(__file__).resolve().parents[3] / "evals" / "tasks"


def _copy_to_temp(source: Path) -> Path:
    tmp = Path(tempfile.mkdtemp(prefix="dyntopo-check-test-"))
    dest = tmp / "workspace"
    shutil.copytree(source, dest)
    return dest


class FixtureDiscoveryTest(unittest.TestCase):
    def test_every_fixture_loads(self) -> None:
        fixture_dirs = expect_mod.discover(TASKS_DIR)
        self.assertGreaterEqual(len(fixture_dirs), 1, "the suite holds at least one fixture")
        for fixture_dir in fixture_dirs:
            loaded = expect_mod.load(fixture_dir)
            self.assertEqual(loaded.name, fixture_dir.name)
            self.assertTrue((fixture_dir / "task.md").is_file(), f"{fixture_dir} has no task.md")
            self.assertTrue(
                (fixture_dir / "workspace").is_dir(), f"{fixture_dir} has no workspace/"
            )
            self.assertTrue((fixture_dir / "solved").is_dir(), f"{fixture_dir} has no solved/")

    def test_shapes_span_several_task_kinds_and_include_one_non_rust_fixture(self) -> None:
        shapes = {expect_mod.load(path).shape for path in expect_mod.discover(TASKS_DIR)}
        for expected in (
            "one-line-fix",
            "repair-loop",
            "refactor-rename-across-files",
            "add-a-test",
            "read-only-question",
            "survey-then-answer",
            "ambiguous-kind",
        ):
            self.assertIn(expected, shapes)
        languages = {expect_mod.load(path).language for path in expect_mod.discover(TASKS_DIR)}
        self.assertIn("python", languages)
        self.assertIn("rust", languages)


class CommandOutcomeCheckerTest(unittest.TestCase):
    """Every `command`-checked fixture: the checker fails on the unsolved
    workspace and passes on the solved one, so the checker is proven against
    both directions rather than only the direction that happens to pass.
    """

    def _fixture(self, name: str) -> expect_mod.FixtureExpect:
        return expect_mod.load(TASKS_DIR / name)

    def _assert_fails_unsolved_passes_solved(self, name: str) -> None:
        loaded = self._fixture(name)
        self.assertEqual(loaded.outcome.check, "command")
        fixture_dir = TASKS_DIR / name

        unsolved = _copy_to_temp(fixture_dir / "workspace")
        try:
            result = checks.check_outcome(loaded.outcome, unsolved, report=None)
            self.assertFalse(result.ok, f"{name}: unsolved workspace unexpectedly passed")
        finally:
            shutil.rmtree(unsolved.parent, ignore_errors=True)

        solved = _copy_to_temp(fixture_dir / "solved")
        try:
            result = checks.check_outcome(loaded.outcome, solved, report=None)
            self.assertTrue(result.ok, f"{name}: solved copy did not pass: {result.detail}")
        finally:
            shutil.rmtree(solved.parent, ignore_errors=True)

    def test_py_oneline_fix(self) -> None:
        self._assert_fails_unsolved_passes_solved("py-oneline-fix")

    def test_rs_careful_verify_edit(self) -> None:
        self._assert_fails_unsolved_passes_solved("rs-careful-verify-edit")

    def test_rs_repair_loop(self) -> None:
        self._assert_fails_unsolved_passes_solved("rs-repair-loop")

    def test_rs_refactor_rename(self) -> None:
        self._assert_fails_unsolved_passes_solved("rs-refactor-rename")

    def test_rs_add_test(self) -> None:
        self._assert_fails_unsolved_passes_solved("rs-add-test")

    def test_rs_ambiguous_kind(self) -> None:
        # This fixture's bar is deliberately lenient (see task.md): the
        # unsolved workspace already passes `cargo test`, so there is no
        # "must fail on unsolved" half to prove — only that the checker
        # reports true on a working tree.
        loaded = self._fixture("rs-ambiguous-kind")
        workspace = _copy_to_temp(TASKS_DIR / "rs-ambiguous-kind" / "workspace")
        try:
            result = checks.check_outcome(loaded.outcome, workspace, report=None)
            self.assertTrue(result.ok, result.detail)
        finally:
            shutil.rmtree(workspace.parent, ignore_errors=True)


def _report(
    *,
    status: str = "completed",
    result: str = "",
    registration_id: str | None,
    recipe: str | None = None,
    blocks: list[str] | None = None,
) -> dict[str, Any]:
    """A minimal stand-in for one `RunReport` JSON object, shaped like
    `crates/polaris_code_core/src/exec/report.rs`'s `Serialize` derive:
    top-level `status`/`result`/`topology`, and `topology.composition.{recipe,
    blocks}` when a topology ran. `registration_id=None` models a
    conversational turn, where `topology` serializes as `null`.
    """
    topology = None
    if registration_id is not None:
        topology = {
            "registration_id": registration_id,
            "decision_id": "00000000-0000-4000-8000-000000000000",
            "fallback_position": 0,
            "rationale": {},
            "candidates": [],
            "composition": (
                {
                    "digest": {"algorithm": "sha256", "hex": "ab" * 32},
                    "recipe": recipe,
                    "blocks": blocks or [],
                    "forced": False,
                    "policy": "polaris-code.composition-registration-projection",
                    "grounds": "test fixture",
                    "prompt_digest": None,
                    "response_digest": None,
                    "evidence": {},
                }
                if recipe is not None
                else None
            ),
        }
    return {
        "session_id": "test-session",
        "turn_number": 1,
        "run_id": "test-run",
        "status": status,
        "result": result,
        "error": None,
        "usage": None,
        "ceiling": None,
        "max_output_tokens": 4096,
        "topology": topology,
        "topology_terminal": None,
    }


class AnswerContainsCheckerTest(unittest.TestCase):
    def test_read_only_question_matches_its_own_canned_report(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-read-only-question")
        good = _report(
            result="resolved the coding task: 1 pass (stopped because goal met)",
            registration_id="code.tree-question",
            recipe="polaris-code.recipe.tree-question",
            blocks=["polaris-code.tree-survey"],
        )
        outcome = checks.check_outcome(loaded.outcome, Path("."), good, tree_unchanged=True)
        self.assertTrue(outcome.ok, outcome.detail)
        plan = checks.check_plan(loaded.plan, good)
        self.assertTrue(plan.ok, plan.detail)

    def test_read_only_question_rejects_a_mutating_answer(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-read-only-question")
        wrong = _report(
            result="resolved the coding task: 1 pass (stopped because goal met)",
            registration_id="code.direct-edit",
            recipe="polaris-code.recipe.direct-verified",
            blocks=["cosmos.verified-step", "cosmos.resolve-residuals"],
        )
        plan = checks.check_plan(loaded.plan, wrong)
        self.assertFalse(plan.ok, "a different registration must not satisfy this fixture's plan")

    def test_read_only_question_rejects_a_changed_tree(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-read-only-question")
        good = _report(
            result="resolved the coding task: 1 pass (stopped because goal met)",
            registration_id="code.tree-question",
            recipe="polaris-code.recipe.tree-question",
            blocks=["polaris-code.tree-survey"],
        )
        outcome = checks.check_outcome(loaded.outcome, Path("."), good, tree_unchanged=False)
        self.assertFalse(outcome.ok, "a route declared tree_unchanged must fail if the tree moved")

    def test_survey_coverage_audit_matches_its_own_canned_report(self) -> None:
        # Conversational route (see task.md): the request is stated in prose
        # and is not directed at code.coverage-audit.
        loaded = expect_mod.load(TASKS_DIR / "rs-survey-coverage-audit")
        self.assertFalse(loaded.plan.expected)
        good = _report(
            result="Unresolved stubs found in src/alpha.rs, src/beta.rs and src/gamma.rs.",
            registration_id=None,
        )
        outcome = checks.check_outcome(loaded.outcome, Path("."), good, tree_unchanged=True)
        self.assertTrue(outcome.ok, outcome.detail)
        plan = checks.check_plan(loaded.plan, good)
        self.assertTrue(plan.ok, plan.detail)

    def test_survey_coverage_audit_rejects_an_answer_missing_a_file(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-survey-coverage-audit")
        incomplete = _report(
            result="Unresolved stubs found in src/alpha.rs and src/beta.rs.",
            registration_id=None,
        )
        outcome = checks.check_outcome(loaded.outcome, Path("."), incomplete, tree_unchanged=True)
        self.assertFalse(outcome.ok, "the answer must name all three files")


class PlanExpectationCheckerTest(unittest.TestCase):
    """`plan.expected = false` fixtures: no topology decision is the only
    passing shape, and any topology decision at all must fail the check.
    """

    def test_conversational_fixtures_pass_only_with_no_topology(self) -> None:
        for name in (
            "rs-repair-loop",
            "rs-refactor-rename",
            "rs-add-test",
            "rs-ambiguous-kind",
            "rs-survey-coverage-audit",
        ):
            with self.subTest(name):
                loaded = expect_mod.load(TASKS_DIR / name)
                self.assertFalse(loaded.plan.expected)
                no_topology = _report(registration_id=None)
                self.assertTrue(checks.check_plan(loaded.plan, no_topology).ok)

                surprise_topology = _report(
                    registration_id="code.direct-edit",
                    recipe="polaris-code.recipe.direct-verified",
                    blocks=["cosmos.verified-step", "cosmos.resolve-residuals"],
                )
                result = checks.check_plan(loaded.plan, surprise_topology)
                self.assertFalse(
                    result.ok, f"{name}: a plan.expected=false fixture must reject any topology"
                )

    def test_topology_fixtures_reject_a_missing_decision(self) -> None:
        for name in (
            "py-oneline-fix",
            "rs-careful-verify-edit",
            "rs-read-only-question",
        ):
            with self.subTest(name):
                loaded = expect_mod.load(TASKS_DIR / name)
                self.assertTrue(loaded.plan.expected)
                result = checks.check_plan(loaded.plan, _report(registration_id=None))
                self.assertFalse(result.ok)


if __name__ == "__main__":
    unittest.main()
