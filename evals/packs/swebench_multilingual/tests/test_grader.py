"""The official grader owns correctness, and its cache must not blur two cells.

SWE-bench caches a verdict by ``(run_id, instance_id)``. The same 43 instances
are attempted once per mode, so a shared run id would serve one mode's cached
verdict for another mode's patch — silently destroying the only comparison this
suite exists to make. That is the property most of these tests defend.

The rest hold the line that an ungradable cell has *no* score rather than a
zero: inventing 0/43 for a cell the grader never judged would report a
measurement that was never made.
"""

from __future__ import annotations

import itertools
import json
import tempfile
import unittest
from pathlib import Path

from ....core.config import MODES
from ....core.process import ReplayRunner, ReplayStep
from ....core.report import EMPTY_PATCH, ERROR, NOT_GRADED, RESOLVED, UNRESOLVED, CellReport
from ..grader import (
    GRADER_MODULE,
    MERGED_PREDICTIONS,
    Grader,
    grader_argv,
    grader_run_id,
    model_name,
    parse_report,
    prediction,
    write_predictions,
)


class GraderIdentityKeepsTheCellsApart(unittest.TestCase):
    """One cell, one run id, one report file."""

    def test_every_cell_of_a_three_sweep_matrix_gets_a_distinct_run_id(self) -> None:
        cells = list(itertools.product(MODES, (1, 2, 3)))
        ids = {grader_run_id("sbml-rust", mode, rep) for mode, rep in cells}
        self.assertEqual(len(ids), len(cells))
        self.assertEqual(len(cells), 9)

    def test_the_run_id_names_the_mode_and_the_repetition(self) -> None:
        self.assertEqual(grader_run_id("sbml", "curated", 2), "sbml-curated-r2")

    def test_two_runs_never_share_a_run_id_either(self) -> None:
        self.assertNotEqual(
            grader_run_id("run-a", "both", 1), grader_run_id("run-b", "both", 1)
        )

    def test_the_report_filename_also_carries_the_cell(self) -> None:
        # The harness names its report after model_name_or_path, so two cells
        # that reused one output directory still land in different files.
        names = {model_name("sbml", mode, rep) for mode, rep in itertools.product(MODES, (1, 2))}
        self.assertEqual(len(names), 6)
        self.assertEqual(model_name("sbml", "freeform", 3), "polaris-code+freeform+r3")

    def test_a_prediction_is_tagged_with_the_cell_that_produced_it(self) -> None:
        record = prediction("tokio-rs__tokio-1", "diff", eval_run_id="sbml", mode="both", repetition=1)
        self.assertEqual(record["instance_id"], "tokio-rs__tokio-1")
        self.assertEqual(record["model_name_or_path"], "polaris-code+both+r1")
        self.assertEqual(record["model_patch"], "diff")


class TheGraderInvocationIsExplicit(unittest.TestCase):
    """The argv keeps the instance list explicit."""

    def test_it_is_the_documented_command(self) -> None:
        argv = grader_argv(
            dataset="SWE-bench/SWE-bench_Multilingual",
            predictions_path=Path("out/predictions/curated/r1.jsonl"),
            instance_ids=["a__b-1", "c__d-2"],
            max_workers=4,
            timeout=1800,
            run_id="sbml-curated-r1",
            python="python",
        )
        self.assertEqual(
            argv,
            (
                "python", "-m", GRADER_MODULE,
                "--dataset_name", "SWE-bench/SWE-bench_Multilingual",
                "--predictions_path", "out/predictions/curated/r1.jsonl",
                "--instance_ids", "a__b-1", "c__d-2",
                "--max_workers", "4",
                "--timeout", "1800",
                "--run_id", "sbml-curated-r1",
            ),
        )

    def test_a_cell_grades_only_the_instances_it_attempted(self) -> None:
        argv = grader_argv(
            dataset="d", predictions_path=Path("p.jsonl"), instance_ids=["only__one-1"],
            max_workers=1, timeout=60, run_id="r",
        )
        self.assertIn("only__one-1", argv)
        self.assertEqual(sum(1 for a in argv if a.endswith("-1")), 1)

    def test_predictions_are_written_one_object_per_line(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "nested" / "predictions.jsonl"
            write_predictions(path, [
                prediction("b__b-2", "p2", eval_run_id="e", mode="both", repetition=1),
                prediction("a__a-1", "p1", eval_run_id="e", mode="both", repetition=1),
            ])
            lines = path.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 2)
            # Written in the order given, so a predictions file matches the
            # order the episodes were attempted in.
            self.assertEqual(json.loads(lines[0])["instance_id"], "b__b-2")
            self.assertEqual(sorted(json.loads(lines[0])), ["instance_id", "model_name_or_path", "model_patch"])


class ADispositionIsNeverInvented(unittest.TestCase):
    """What the report says, and what it refuses to say."""

    def report(self, **kwargs) -> CellReport:
        base = dict(
            mode="curated", repetition=1, run_id="r",
            resolved_ids=frozenset({"a__a-1"}),
            unresolved_ids=frozenset({"b__b-2"}),
            empty_patch_ids=frozenset({"c__c-3"}),
            error_ids=frozenset({"d__d-4"}),
            graded=True, exit_code=0,
        )
        base.update(kwargs)
        return CellReport(**base)  # type: ignore[arg-type]

    def test_each_official_disposition_is_reported_as_itself(self) -> None:
        report = self.report()
        self.assertEqual(report.disposition("a__a-1"), RESOLVED)
        self.assertEqual(report.disposition("b__b-2"), UNRESOLVED)
        self.assertEqual(report.disposition("c__c-3"), EMPTY_PATCH)
        self.assertEqual(report.disposition("d__d-4"), ERROR)

    def test_an_instance_the_grader_never_mentioned_counts_as_unresolved(self) -> None:
        # Fixed denominator: a missing verdict must not shrink it.
        self.assertEqual(self.report().disposition("never__seen-9"), UNRESOLVED)

    def test_an_ungraded_cell_reports_not_graded_for_everything(self) -> None:
        ungraded = self.report(graded=False)
        self.assertEqual(ungraded.disposition("a__a-1"), NOT_GRADED)
        self.assertEqual(ungraded.disposition("never__seen-9"), NOT_GRADED)

    def test_a_report_round_trips_through_its_json_form(self) -> None:
        report = self.report(note="something")
        self.assertEqual(CellReport.from_json(report.to_json()), report)

    def test_the_official_field_name_for_empty_patches_is_understood(self) -> None:
        # The harness spells it `empty_patch_instances_ids`; the shorter alias
        # is accepted too so a grader revision that renames it does not lose
        # every empty-patch verdict.
        official = parse_report(
            {"resolved_ids": ["a__a-1"], "empty_patch_instances_ids": ["c__c-3"]},
            mode="both", repetition=1, run_id="r", exit_code=0,
        )
        self.assertEqual(official.empty_patch_ids, frozenset({"c__c-3"}))
        alias = parse_report(
            {"empty_patch_ids": ["c__c-3"]}, mode="both", repetition=1, run_id="r", exit_code=0,
        )
        self.assertEqual(alias.empty_patch_ids, frozenset({"c__c-3"}))


class AGraderThatWillNotRunLeavesTheCellUnscored(unittest.TestCase):
    """The honest outcome for an ungradable cell is no score, not zero."""

    def grade(self, steps, tmp: Path) -> CellReport:
        # The runner creates this directory before grading; the grader writes
        # its report into it, so the fixture has to create it too.
        (tmp / "grader").mkdir(exist_ok=True)
        grader = Grader(ReplayRunner(steps), dataset="d", max_workers=2, timeout=30)
        return grader.grade(
            eval_run_id="sbml", mode="curated", repetition=1,
            patches={"a__a-1": "diff --git a/x b/x\n"}, instance_ids=["a__a-1"], run_dir=tmp,
        )

    def test_a_nonzero_exit_leaves_the_cell_ungraded_with_a_note(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            report = self.grade(
                [ReplayStep(match=("run_evaluation",), returncode=1, stderr="docker is not running")],
                tmp,
            )
            self.assertFalse(report.graded)
            self.assertEqual(report.exit_code, 1)
            self.assertIn("docker is not running", report.note or "")
            self.assertEqual(report.resolved_ids, frozenset())

    def test_a_grader_that_exits_zero_but_writes_nothing_is_still_ungraded(self) -> None:
        # The most dangerous shape: a success exit with no verdicts would look
        # like a clean sweep of zero resolved.
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            report = self.grade([ReplayStep(match=("run_evaluation",), stdout="done\n")], tmp)
            self.assertFalse(report.graded)
            self.assertIn("wrote no report", report.note or "")

    def test_a_timeout_leaves_the_cell_ungraded(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            report = self.grade(
                [ReplayStep(match=("run_evaluation",), returncode=-62, timed_out=True)], tmp
            )
            self.assertFalse(report.graded)

    def test_a_real_report_file_is_read_back_through_the_measured_path(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            body = json.dumps({
                "resolved_ids": ["a__a-1"],
                "unresolved_ids": ["b__b-2"],
                "empty_patch_instances_ids": [],
                "error_ids": [],
            })
            report = self.grade(
                [ReplayStep(
                    match=("run_evaluation",),
                    writes=((f"{model_name('sbml', 'curated', 1)}.{grader_run_id('sbml', 'curated', 1)}.json", body),),
                )],
                tmp,
            )
            self.assertTrue(report.graded)
            self.assertEqual(report.resolved_ids, frozenset({"a__a-1"}))
            self.assertEqual(report.run_id, "sbml-curated-r1")

    def test_a_renamed_report_is_still_found(self) -> None:
        # The filename has moved between grader revisions; a run must not lose
        # its verdicts to a rename it could have recovered from.
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            body = json.dumps({"resolved_ids": ["a__a-1"]})
            report = self.grade(
                [ReplayStep(
                    match=("run_evaluation",),
                    writes=((f"report.{grader_run_id('sbml', 'curated', 1)}.evaluation.json", body),),
                )],
                tmp,
            )
            self.assertTrue(report.graded)
            self.assertEqual(report.resolved_ids, frozenset({"a__a-1"}))


class TheMergedPredictionsFileFollowsTheGradingOrder(unittest.TestCase):
    """One whole-run ``predictions.jsonl``, in the order the cells were graded."""

    def test_every_graded_cell_lands_in_it_once_and_in_order(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            (tmp / "grader").mkdir()
            steps = [ReplayStep(match=("run_evaluation",)) for _ in range(2)]
            grader = Grader(ReplayRunner(steps), dataset="d", max_workers=1, timeout=30)
            for mode in ("curated", "freeform"):
                grader.grade(
                    eval_run_id="sbml", mode=mode, repetition=1,
                    patches={"a__a-1": f"patch for {mode}"}, instance_ids=["a__a-1"],
                    run_dir=tmp,
                )
            grader.write_merged_predictions(tmp)
            lines = (tmp / MERGED_PREDICTIONS).read_text(encoding="utf-8").splitlines()
            self.assertEqual(
                [json.loads(line)["model_name_or_path"] for line in lines],
                ["polaris-code+curated+r1", "polaris-code+freeform+r1"],
            )

    def test_each_cell_also_keeps_its_own_predictions_file(self) -> None:
        # The grader is handed one cell's file, never the merged one, because a
        # merged file would put the same instance in three times.
        with tempfile.TemporaryDirectory() as raw:
            tmp = Path(raw)
            (tmp / "grader").mkdir()
            grader = Grader(
                ReplayRunner([ReplayStep(match=("run_evaluation",))]),
                dataset="d", max_workers=1, timeout=30,
            )
            grader.grade(
                eval_run_id="sbml", mode="both", repetition=2,
                patches={"a__a-1": "p"}, instance_ids=["a__a-1"], run_dir=tmp,
            )
            self.assertTrue((tmp / "predictions" / "both" / "r2.jsonl").is_file())


if __name__ == "__main__":
    unittest.main()
