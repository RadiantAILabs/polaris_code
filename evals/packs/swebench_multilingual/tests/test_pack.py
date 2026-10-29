"""What this pack supplies, and what the core must therefore never assume.

Everything asserted here is a property of SWE-bench Multilingual: its default
cost ceiling, the five pins its summary repeats, the artifact it adds to a run
directory, and its split and licence
policy. A second dataset would answer all of these differently, which is the
whole reason they live in a pack.
"""

from __future__ import annotations

import argparse
import unittest

from ....core.budget import BudgetExceeded, CostGuard
from ....core.config import MODES
from ....packs import UnknownPack, load
from ..grader import MERGED_PREDICTIONS
from ..pack import DEFAULT_API_COST_CEILING_USD, SwebenchMultilingualPack
from ..suite import SUITE_DENOMINATOR


class TheCostCeilingBelongsToThisDataset(unittest.TestCase):
    """Callers choose a nonzero ceiling explicitly for each run."""

    def test_the_default_refuses_a_paid_sweep(self) -> None:
        one_sweep = SUITE_DENOMINATOR * len(MODES)
        with self.assertRaises(BudgetExceeded):
            CostGuard(ceiling_usd=DEFAULT_API_COST_CEILING_USD).check_projection(one_sweep)

    def test_the_ceiling_is_the_packs_default_not_the_cores(self) -> None:
        self.assertEqual(
            SwebenchMultilingualPack().default_api_cost_ceiling_usd(),
            DEFAULT_API_COST_CEILING_USD,
        )


class TheSummaryShapeIsPartOfTheEvidence(unittest.TestCase):
    """The pack declares which suite keys its summaries repeat."""

    def test_the_merged_predictions_file_is_digested_as_this_packs_artifact(self) -> None:
        self.assertEqual(SwebenchMultilingualPack().digested_outputs(), (MERGED_PREDICTIONS,))


class ThePolicyRecordsTheSplitAndTheOpenLicenceQuestion(unittest.TestCase):
    """Split and licence terms belong to the dataset, and are written down."""

    def setUp(self) -> None:
        self.policy = SwebenchMultilingualPack().policy()

    def test_only_the_test_split_is_used_and_nothing_is_held_out(self) -> None:
        self.assertEqual(self.policy["split"], "test")
        self.assertTrue(self.policy["split_is_the_only_one_used"])
        self.assertIsNone(self.policy["held_out_split"])
        self.assertEqual(self.policy["language_filter"], "Rust")

    def test_the_unestablished_licence_is_recorded_as_unknown_with_a_reason(self) -> None:
        # A licence name nobody verified would be worse than an open question.
        self.assertTrue(self.policy["problem_statement_checked_in"])
        self.assertIsNone(self.policy["problem_statement_licence"])
        self.assertIn("upstream dataset", self.policy["problem_statement_licence_note"])

    def test_grading_is_official_and_the_images_are_upstream(self) -> None:
        self.assertTrue(self.policy["grading_is_official"])
        self.assertTrue(self.policy["images_are_upstream"])


class ThePinsOnTheCommandLineAreAssertedNotApplied(unittest.TestCase):
    """A stated pin that disagrees with the checked-in suite refuses the run."""

    def setUp(self) -> None:
        self.pack = SwebenchMultilingualPack()
        self.suite = self.pack.load_suite()

    def _options(self, **overrides: object) -> argparse.Namespace:
        stated = dict(
            dataset=None, dataset_revision=None, task_repo_revision=None,
            grader_revision=None, language=None,
        )
        stated.update(overrides)
        return argparse.Namespace(**stated)

    def test_matching_pins_are_accepted(self) -> None:
        options = self._options(
            dataset=self.suite.dataset,
            dataset_revision=self.suite.dataset_revision,
            language="Rust",
        )
        self.assertIsNone(self.pack.check_run_arguments(options, self.suite))

    def test_a_disagreeing_revision_names_the_flag_and_both_values(self) -> None:
        options = self._options(dataset_revision="0" * 40)
        message = self.pack.check_run_arguments(options, self.suite)
        self.assertIsNotNone(message)
        self.assertIn("--dataset-revision", message or "")
        self.assertIn(self.suite.dataset_revision, message or "")


class ThePackIsReachableByName(unittest.TestCase):
    """The registry is how a run directory's ``dataset_pack`` is resolved back."""

    def test_the_registry_returns_this_pack(self) -> None:
        pack = load("swebench-multilingual")
        self.assertIsInstance(pack, SwebenchMultilingualPack)
        self.assertEqual(pack.name, "swebench-multilingual")
        self.assertEqual(pack.version, "1.1.0")

    def test_an_unknown_name_is_refused_rather_than_guessed(self) -> None:
        with self.assertRaises(UnknownPack):
            load("swebench-pro")


if __name__ == "__main__":
    unittest.main()
