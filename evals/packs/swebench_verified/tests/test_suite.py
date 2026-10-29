"""The checked-in Verified pin is complete, stable, and agent-visible."""

from __future__ import annotations

import unittest

from ..suite import SUITE_DENOMINATOR, Suite


class VerifiedSuiteIsTheOfficialPinnedSet(unittest.TestCase):
    """Guard the dataset identity and the fixed 500-task denominator."""

    def setUp(self) -> None:
        self.suite = Suite.load()

    def test_the_full_human_validated_set_is_pinned(self) -> None:
        self.assertEqual(len(self.suite.instances), SUITE_DENOMINATOR)
        self.assertEqual(self.suite.denominator, 500)
        self.assertEqual(self.suite.dataset, "SWE-bench/SWE-bench_Verified")
        self.assertEqual(self.suite.dataset_revision, "78f471bf655a3137b2e8a75af1501690ec009ec3")
        self.assertEqual(
            self.suite.dataset_file_sha256,
            "030cfd7f2a704c4c0226e7f104c725a3b41230b1d3517f9c915ad7ea5be3fa25",
        )

    def test_the_task_rows_have_the_expected_content_lock(self) -> None:
        self.assertEqual(
            self.suite.content_digest(),
            "fb669f713628236df3f23af68327ce0f1eeba006d3000e4204b0a84d6e1e0300",
        )
        self.assertEqual(self.suite.instance_ids, tuple(sorted(self.suite.instance_ids)))
        self.assertEqual(len(set(self.suite.instance_ids)), 500)

    def test_the_gold_patch_is_an_upstream_patch_for_a_pinned_instance(self) -> None:
        self.assertEqual(self.suite.gold.instance_id, "sympy__sympy-22914")
        self.assertIn(self.suite.gold.instance_id, self.suite.instance_ids)
        self.assertTrue(self.suite.gold.model_patch.startswith("diff --git "))


if __name__ == "__main__":
    unittest.main()
