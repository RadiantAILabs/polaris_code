"""``summary.json`` refuses to report a number nobody measured.

Three refusals, each tested here because the easy alternative is a lie a reader
could not detect:

* a cell that did not attempt *and* grade all of its pinned instances has no
  resolved rate, because the denominator is fixed;
* a replayed run is a ``plumbing-demonstration`` whatever its numbers say; and
* a difference that does not clear all four conditions of the evidence bar is
  ``not distinguished at this budget``, and every mode pair is reported rather
  than only the flattering one.
"""

from __future__ import annotations

import dataclasses
import unittest

from ..collate import (
    CONCLUSION_CREDIBLE,
    CONCLUSION_INCOMPLETE,
    CONCLUSION_NOT_DISTINGUISHED,
    CONCLUSION_PLUMBING,
    render,
    summarise,
)
from ..config import EVIDENCE_BAR, SUMMARY_SCHEMA
from ..process import REPLAY, SUBPROCESS

#: A small pinned list, so a "complete" cell is reachable inside a test.
PINNED = tuple(f"repo__task-{n}" for n in range(10))
MODES = ("curated", "freeform", "both")


def manifest(
    *,
    modes=MODES,
    repetitions: int = 1,
    denominator: int = len(PINNED),
    process_runner: str = SUBPROCESS,
    pinned=PINNED,
    iteration_profile: str | None = None,
    instance_ids=(),
) -> dict:
    """A manifest carrying only what :func:`summarise` reads."""
    return {
        "config": {
            "eval_run_id": "sbml-test",
            "model": "anthropic/claude-sonnet-4-6",
            "modes": list(modes),
            "repetitions": repetitions,
            "analysis_seed": 1729,
            "process_runner": process_runner,
            "iteration_profile": iteration_profile,
            "instance_ids": list(instance_ids),
            "episode_timeout_seconds": 1800,
            "evidence_bar": dataclasses.asdict(EVIDENCE_BAR),
        },
        "suite": {
            # Deliberately not any real dataset's keys: the core copies the
            # keys a pack names and knows nothing about what they mean.
            "dataset": "example/benchmark",
            "dataset_revision": "revision-a",
            "denominator": denominator,
            "instance_ids": list(pinned),
        },
    }


def episodes(*, modes=MODES, repetitions: int = 1, instances=PINNED, cost: float | None = 0.01, seconds: float = 1.5):
    """One retained attempt per cell, with readable accounting."""
    out = []
    for repetition in range(1, repetitions + 1):
        for mode in modes:
            for instance in instances:
                out.append({
                    "mode": mode,
                    "repetition": repetition,
                    "instance_id": instance,
                    "attempt": 1,
                    "inference_seconds": seconds,
                    "usage": {
                        "input_tokens": 100, "output_tokens": 10,
                        "cache_read_tokens": 1, "cache_creation_tokens": 2,
                        "total_tokens": 113, "estimated_cost_usd": cost,
                    },
                })
    return out


def cells(resolved_by_mode: dict[str, set[str]], *, repetitions: int = 1, graded=True, instances=PINNED, modes=MODES):
    """Grader cells naming which instances each mode resolved."""
    out = []
    for repetition in range(1, repetitions + 1):
        for mode in modes:
            resolved = set(resolved_by_mode.get(mode, ()))
            out.append({
                "mode": mode,
                "repetition": repetition,
                "run_id": f"sbml-test-{mode}-r{repetition}",
                "graded": graded,
                "exit_code": 0,
                "note": None,
                "resolved_ids": sorted(resolved),
                "unresolved_ids": sorted(set(instances) - resolved),
                "empty_patch_ids": [],
                "error_ids": [],
            })
    return out


class TheDenominatorIsFixedAndACellMustEarnItsRate(unittest.TestCase):
    """A rate exists only when every pinned instance was attempted and graded."""

    def test_a_complete_cell_scores_over_the_pinned_denominator(self) -> None:
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(),
            grader_cells=cells({"curated": {PINNED[0], PINNED[1]}}),
        )
        curated = next(c for c in summary["cells"] if c["mode"] == "curated")
        self.assertTrue(curated["complete"])
        self.assertEqual(curated["resolved"], 2)
        self.assertEqual(curated["denominator"], 10)
        self.assertAlmostEqual(curated["resolved_rate"], 20.0, places=6)

    def test_a_subset_run_has_no_rate_at_all(self) -> None:
        # The case the budget gate makes routine: a handful of instances is a
        # plumbing run, and `2 resolved of 3 attempted` is not a score.
        subset = PINNED[:3]
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(instances=subset),
            grader_cells=cells({"curated": {subset[0]}}, instances=subset),
        )
        for cell in summary["cells"]:
            self.assertFalse(cell["complete"])
            self.assertIsNone(cell["resolved_rate"])
        self.assertFalse(summary["complete"])
        self.assertEqual(summary["conclusion"], CONCLUSION_INCOMPLETE)
        self.assertIn("did not attempt and grade all 10", summary["conclusion_note"])

    def test_one_missing_instance_is_enough_to_withhold_the_rate(self) -> None:
        # The most tempting shortcut: 9 of 10 looks complete enough, and
        # scoring it would quietly raise the rate by dropping a hard task.
        attempted = PINNED[:-1]
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(instances=attempted),
            grader_cells=cells({"curated": {PINNED[0]}}, instances=attempted),
        )
        curated = next(c for c in summary["cells"] if c["mode"] == "curated")
        self.assertEqual(curated["attempted"], 9)
        self.assertIsNone(curated["resolved_rate"])
        self.assertFalse(curated["complete"])

    def test_an_ungraded_cell_has_no_rate_even_when_every_episode_ran(self) -> None:
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(),
            grader_cells=cells({}, graded=False),
        )
        for cell in summary["cells"]:
            self.assertEqual(cell["attempted"], 10)
            self.assertEqual(cell["graded"], 0)
            self.assertIsNone(cell["resolved_rate"])

    def test_an_instance_the_grader_did_not_mention_is_unresolved_not_absent(self) -> None:
        partial = cells({"curated": {PINNED[0]}})
        for cell in partial:
            cell["unresolved_ids"] = []
        summary = summarise(manifest=manifest(), episodes=episodes(), grader_cells=partial)
        curated = next(c for c in summary["cells"] if c["mode"] == "curated")
        # Graded but unmentioned instances are missing from the report, so the
        # cell is not complete and scores nothing.
        self.assertEqual(curated["graded"], 1)
        self.assertIsNone(curated["resolved_rate"])


class AReplayedRunIsNeverAMeasurement(unittest.TestCase):
    """``process_runner = replay`` overrides the arithmetic."""

    def test_a_replayed_sweep_that_looks_perfect_is_still_a_demonstration(self) -> None:
        summary = summarise(
            manifest=manifest(process_runner=REPLAY, repetitions=3),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        # Complete cells, a 100-point spread, three sweeps — and it still
        # refuses to be evidence, because nothing was executed.
        self.assertTrue(summary["complete"])
        self.assertEqual(summary["conclusion"], CONCLUSION_PLUMBING)
        self.assertIn("no money was spent", summary["conclusion_note"])
        self.assertEqual(summary["process_runner"], REPLAY)

    def test_the_same_numbers_executed_would_have_been_credible(self) -> None:
        # The control for the test above: only process_runner differs, so the
        # refusal is demonstrably the replay flag and not the numbers.
        summary = summarise(
            manifest=manifest(process_runner=SUBPROCESS, repetitions=3),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_CREDIBLE)


class TheEvidenceBarIsEnforcedNotDescribed(unittest.TestCase):
    """All four conditions, together, and every pair reported."""

    def test_one_sweep_is_never_credible_however_large_the_gap(self) -> None:
        summary = summarise(
            manifest=manifest(repetitions=1),
            episodes=episodes(),
            grader_cells=cells({"curated": set(PINNED)}),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_NOT_DISTINGUISHED)
        for comparison in summary["comparisons"]:
            self.assertFalse(comparison["credible"])
            self.assertFalse(comparison["meets_minimum_sweeps"])

    def test_a_gap_under_five_points_is_not_distinguished(self) -> None:
        # One instance in ten is ten points; to land under the bar the modes
        # must differ by nothing at all here, so the gap is tested at the
        # comparison level instead.
        summary = summarise(
            manifest=manifest(repetitions=3),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": {PINNED[0]}, "freeform": {PINNED[0]}, "both": {PINNED[0]}}, repetitions=3),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_NOT_DISTINGUISHED)
        for comparison in summary["comparisons"]:
            self.assertAlmostEqual(comparison["bootstrap"]["difference_points"], 0.0, places=9)
            self.assertFalse(comparison["meets_minimum_difference"])

    def test_every_mode_pair_is_reported_in_canonical_order(self) -> None:
        # All three are always present so a caller cannot select only the
        # favorable pair.
        summary = summarise(
            manifest=manifest(repetitions=3),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        self.assertEqual(
            [c["pair"] for c in summary["comparisons"]],
            ["curated-freeform", "curated-both", "freeform-both"],
        )

    def test_a_credible_difference_names_all_four_conditions_as_met(self) -> None:
        summary = summarise(
            manifest=manifest(repetitions=3),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        winner = next(c for c in summary["comparisons"] if c["pair"] == "curated-freeform")
        self.assertTrue(winner["meets_minimum_sweeps"])
        self.assertTrue(winner["meets_minimum_difference"])
        self.assertTrue(winner["direction_consistent"])
        self.assertTrue(winner["bootstrap"]["excludes_zero"])
        self.assertTrue(winner["credible"])
        self.assertEqual(summary["conclusion"], CONCLUSION_CREDIBLE)
        self.assertIn("this agent commit, model, budget and pinned suite only", summary["conclusion_note"])

    def test_an_incomplete_run_can_never_be_credible(self) -> None:
        # Eligibility is gated on completeness, so a halted sweep cannot report
        # a difference even with three repetitions and a wide gap.
        attempted = PINNED[:-1]
        summary = summarise(
            manifest=manifest(repetitions=3),
            episodes=episodes(repetitions=3, instances=attempted),
            grader_cells=cells({"curated": set(attempted)}, repetitions=3, instances=attempted),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_INCOMPLETE)
        for comparison in summary["comparisons"]:
            self.assertFalse(comparison["eligible"])
            self.assertFalse(comparison["credible"])

    def test_the_bar_in_force_is_recorded_in_the_summary(self) -> None:
        summary = summarise(manifest=manifest(), episodes=episodes(), grader_cells=cells({}))
        self.assertEqual(summary["evidence_bar"], dataclasses.asdict(EVIDENCE_BAR))
        self.assertEqual(summary["evidence_bar"]["min_sweeps"], 3)
        self.assertEqual(summary["evidence_bar"]["min_absolute_difference_points"], 5.0)


class CostTotalsNeverUnderstateSpend(unittest.TestCase):
    """An unknown cost keeps the total unknown rather than partial."""

    def test_a_known_total_is_the_sum_of_the_episodes(self) -> None:
        summary = summarise(manifest=manifest(), episodes=episodes(cost=0.01), grader_cells=cells({}))
        self.assertEqual(summary["totals"]["episodes"], 30)
        self.assertAlmostEqual(summary["totals"]["measured_cost_usd"], 0.30, places=6)
        self.assertEqual(summary["totals"]["unknown_cost_episodes"], 0)

    def test_one_unpriced_episode_makes_the_total_unknown(self) -> None:
        records = episodes()
        records[0]["usage"]["estimated_cost_usd"] = None
        summary = summarise(manifest=manifest(), episodes=records, grader_cells=cells({}))
        self.assertIsNone(summary["totals"]["measured_cost_usd"])
        self.assertEqual(summary["totals"]["unknown_cost_episodes"], 1)
        # What *was* priced is still reported, so the partial sum is visible
        # without being presented as the total.
        self.assertAlmostEqual(summary["totals"]["priced_cost_usd"], 0.29, places=6)

    def test_an_unmeasured_episode_also_makes_the_total_unknown(self) -> None:
        records = episodes()
        records[0]["usage"] = None
        summary = summarise(manifest=manifest(), episodes=records, grader_cells=cells({}))
        self.assertIsNone(summary["totals"]["measured_cost_usd"])

    def test_tokens_are_summed_by_tier(self) -> None:
        summary = summarise(manifest=manifest(), episodes=episodes(), grader_cells=cells({}))
        self.assertEqual(
            summary["totals"]["tokens"],
            {"input": 3000, "output": 300, "cache_read": 30, "cache_creation": 60},
        )

    def test_a_retried_episode_is_counted_once_in_the_cell(self) -> None:
        # Both attempts stay in episodes.jsonl; only the later one enters the
        # cell's arithmetic.
        records = episodes()
        first = dict(records[0])
        first["attempt"] = 1
        records[0] = {**records[0], "attempt": 2}
        summary = summarise(manifest=manifest(), episodes=[first, *records], grader_cells=cells({}))
        curated = next(c for c in summary["cells"] if c["mode"] == "curated")
        self.assertEqual(curated["attempted"], 10)
        # The totals still see every retained attempt, because that is what was
        # actually spent.
        self.assertEqual(summary["totals"]["episodes"], 31)


class TheSummaryIsDeterministicallyRendered(unittest.TestCase):
    """``recompute`` compares bytes, so rendering must be stable."""

    def test_the_same_inputs_render_identical_bytes(self) -> None:
        args = dict(manifest=manifest(), episodes=episodes(), grader_cells=cells({"both": {PINNED[3]}}))
        self.assertEqual(render(summarise(**args)), render(summarise(**args)))

    def test_the_rendering_is_sorted_and_newline_terminated(self) -> None:
        rendered = render(summarise(manifest=manifest(), episodes=episodes(), grader_cells=cells({})))
        self.assertTrue(rendered.endswith("\n"))
        self.assertIn('"schema": "' + SUMMARY_SCHEMA + '"', rendered)

    def test_the_summary_names_the_suite_it_scored(self) -> None:
        summary = summarise(manifest=manifest(), episodes=episodes(), grader_cells=cells({}))
        self.assertEqual(summary["suite"], {"denominator": 10})
        self.assertEqual(summary["schema"], SUMMARY_SCHEMA)

    def test_a_pack_chooses_which_pins_the_summary_repeats(self) -> None:
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(),
            grader_cells=cells({}),
            suite_keys=("dataset", "dataset_revision", "denominator"),
        )
        self.assertEqual(
            summary["suite"],
            {"dataset": "example/benchmark", "dataset_revision": "revision-a", "denominator": 10},
        )

    def test_a_pin_the_manifest_never_carried_is_omitted_not_null(self) -> None:
        # A pack that adds a pin must not make every older run directory
        # irreproducible, and `null` would be a claim that the run recorded an
        # empty value rather than none.
        summary = summarise(
            manifest=manifest(),
            episodes=episodes(),
            grader_cells=cells({}),
            suite_keys=("denominator", "licence_added_later"),
        )
        self.assertEqual(summary["suite"], {"denominator": 10})


if __name__ == "__main__":
    unittest.main()
