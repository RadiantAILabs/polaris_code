"""The bootstrap must be deterministic, and it must cluster by task.

Two properties, both load-bearing for the evidence bar:

* **Determinism.** ``recompute`` byte-reproduces ``summary.json``, and the
  intervals are in it. A bootstrap that consulted the clock, the platform or a
  hash seed would make a recorded summary unreproducible.
* **Clustering.** The same instance is attempted once per mode, so episodes are
  paired rather than independent. Resampling the 30 episodes instead of the 10
  task clusters would shrink every interval and manufacture credibility.
"""

from __future__ import annotations

import unittest

from ..statistics import Interval, paired_cluster_bootstrap

#: A small cluster set; the ids are ordered so the fixed ordering is visible.
CLUSTERS = tuple(f"repo__task-{n}" for n in range(10))


def flags(pattern: str, repetitions: int = 1) -> dict[str, list[bool]]:
    """Per-cluster flags from a ``1``/``0`` string, repeated ``repetitions``."""
    return {
        cluster: [bit == "1"] * repetitions
        for cluster, bit in zip(CLUSTERS, pattern)
    }


def bootstrap(a: str, b: str, *, seed: int = 1729, repetitions: int = 1, resamples: int = 2000) -> Interval:
    """Bootstrap two modes described by ``1``/``0`` patterns."""
    return paired_cluster_bootstrap(
        cluster_ids=CLUSTERS,
        a=flags(a, repetitions),
        b=flags(b, repetitions),
        seed=seed,
        resamples=resamples,
        confidence=0.95,
    )


class TheProcedureIsDeterministic(unittest.TestCase):
    """Same inputs and same seed, same interval, every time and everywhere."""

    def test_the_same_seed_reproduces_the_interval_exactly(self) -> None:
        first = bootstrap("1111100000", "0000000000")
        second = bootstrap("1111100000", "0000000000")
        self.assertEqual(first, second)
        self.assertEqual(first.to_json(), second.to_json())

    def test_a_different_seed_is_a_different_resample(self) -> None:
        # Not a claim that the answer should change much — only that the seed
        # is actually consumed, so recording it means something.
        self.assertNotEqual(
            bootstrap("1111100000", "0000000000", seed=1).to_json(),
            bootstrap("1111100000", "0000000000", seed=2).to_json(),
        )

    def test_cluster_order_does_not_change_the_answer(self) -> None:
        # The procedure sorts by instance id, so a caller that passes the
        # clusters in a different order still gets the recorded interval.
        ordered = paired_cluster_bootstrap(
            cluster_ids=CLUSTERS, a=flags("1111100000"), b=flags("0000000000"),
            seed=7, resamples=500, confidence=0.95,
        )
        shuffled = paired_cluster_bootstrap(
            cluster_ids=tuple(reversed(CLUSTERS)), a=flags("1111100000"), b=flags("0000000000"),
            seed=7, resamples=500, confidence=0.95,
        )
        self.assertEqual(ordered.to_json(), shuffled.to_json())

    def test_the_recorded_configuration_is_carried_in_the_result(self) -> None:
        interval = bootstrap("1000000000", "0000000000", seed=99, resamples=250)
        self.assertEqual(interval.seed, 99)
        self.assertEqual(interval.resamples, 250)
        self.assertEqual(interval.confidence, 0.95)
        self.assertEqual(interval.clusters, len(CLUSTERS))


class ThePointEstimateIsTheDifferenceInResolvedRate(unittest.TestCase):
    """The headline number, in percentage points over the fixed denominator."""

    def test_five_of_ten_against_none_is_fifty_points(self) -> None:
        self.assertAlmostEqual(bootstrap("1111100000", "0000000000").difference_points, 50.0, places=6)

    def test_identical_modes_differ_by_nothing(self) -> None:
        interval = bootstrap("1010101010", "1010101010")
        self.assertAlmostEqual(interval.difference_points, 0.0, places=9)
        self.assertFalse(interval.excludes_zero)

    def test_the_difference_is_signed(self) -> None:
        self.assertLess(bootstrap("0000000000", "1111100000").difference_points, 0.0)

    def test_repetitions_average_within_a_cluster_before_differencing(self) -> None:
        # A cluster resolved in 2 of 3 repetitions contributes 2/3, not 1. This
        # is what makes three sweeps more informative than one rather than
        # three times as confident about the same single observation.
        mixed = {CLUSTERS[0]: [True, True, False]}
        none = {CLUSTERS[0]: [False, False, False]}
        interval = paired_cluster_bootstrap(
            cluster_ids=(CLUSTERS[0],), a=mixed, b=none, seed=1, resamples=100, confidence=0.95,
        )
        self.assertAlmostEqual(interval.difference_points, 100.0 * 2 / 3, places=6)


class TheFixedDenominatorSurvivesResampling(unittest.TestCase):
    """A task with no attempt counts as unresolved, and cannot be dropped."""

    def test_a_cluster_with_no_flags_counts_as_zero(self) -> None:
        # Not as absent: dropping it would divide by 9 instead of 10 and raise
        # both modes' rates.
        partial = {CLUSTERS[0]: [True]}
        empty: dict[str, list[bool]] = {}
        interval = paired_cluster_bootstrap(
            cluster_ids=CLUSTERS, a=partial, b=empty, seed=3, resamples=200, confidence=0.95,
        )
        self.assertAlmostEqual(interval.difference_points, 10.0, places=6)
        self.assertEqual(interval.clusters, 10)

    def test_an_empty_cluster_set_is_a_zero_interval_rather_than_a_crash(self) -> None:
        interval = paired_cluster_bootstrap(
            cluster_ids=(), a={}, b={}, seed=1, resamples=10, confidence=0.95,
        )
        self.assertEqual(interval.clusters, 0)
        self.assertFalse(interval.excludes_zero)


class ExcludingZeroIsWhatTheEvidenceBarReads(unittest.TestCase):
    """The interval, not the point estimate, decides credibility."""

    def test_a_unanimous_difference_excludes_zero(self) -> None:
        # Every cluster resolves under a and none under b, so no resample can
        # produce a difference of zero.
        interval = bootstrap("1111111111", "0000000000")
        self.assertTrue(interval.excludes_zero)
        self.assertGreater(interval.low_points, 0.0)

    def test_a_one_cluster_difference_in_ten_does_not_exclude_zero(self) -> None:
        # Many resamples omit the single differing cluster entirely, so the
        # interval reaches zero — which is the whole point of the rule.
        self.assertFalse(bootstrap("1000000000", "0000000000").excludes_zero)

    def test_an_interval_wholly_below_zero_also_excludes_it(self) -> None:
        interval = bootstrap("0000000000", "1111111111")
        self.assertTrue(interval.excludes_zero)
        self.assertLess(interval.high_points, 0.0)

    def test_the_interval_brackets_the_point_estimate(self) -> None:
        interval = bootstrap("1111100000", "0000011111")
        self.assertLessEqual(interval.low_points, interval.difference_points)
        self.assertGreaterEqual(interval.high_points, interval.difference_points)


class ANonsenseConfigurationIsRefused(unittest.TestCase):
    """A silently degenerate bootstrap would be worse than none."""

    def test_zero_resamples_is_refused(self) -> None:
        with self.assertRaises(ValueError):
            bootstrap("1", "0", resamples=0)

    def test_a_confidence_outside_the_open_unit_interval_is_refused(self) -> None:
        for confidence in (0.0, 1.0, -0.5, 1.5):
            with self.subTest(confidence=confidence):
                with self.assertRaises(ValueError):
                    paired_cluster_bootstrap(
                        cluster_ids=CLUSTERS, a=flags("1"), b=flags("0"),
                        seed=1, resamples=10, confidence=confidence,
                    )


if __name__ == "__main__":
    unittest.main()
