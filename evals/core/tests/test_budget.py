"""The cost guard prevents a run from exceeding its configured limit.

The guard is the core's and these tests are about the guard. Whether a
*particular* dataset's configured ceiling admits a particular sweep is that
pack's question, and
``evals.packs.swebench_multilingual.tests.test_pack`` asks it.

The rest defend the one sharp edge in the usage contract: ``null`` is UNKNOWN,
never zero.
"""

from __future__ import annotations

import unittest

from ..budget import HALT_CEILING, HALT_UNKNOWN_COST, BudgetExceeded, CostGuard
from ..config import PLANNING_COST_PER_EPISODE_USD
from ..usage import RunUsage


def usage(cost: float | None) -> RunUsage:
    """One episode's accounting with a chosen estimated cost."""
    return RunUsage(
        model_id="anthropic/claude-sonnet-4-6",
        input_tokens=100,
        output_tokens=10,
        cache_read_tokens=0,
        cache_creation_tokens=0,
        total_tokens=110,
        estimated_cost_usd=cost,
        pricing_basis="recorded-price-table",
        pricing=None if cost is None else {"family": "sonnet"},
    )


class ProjectionRefusesAnOverrun(unittest.TestCase):
    """``check_projection`` refuses to *start* an unaffordable run."""

    def test_the_refusal_names_both_numbers_and_the_flag_that_moves_it(self) -> None:
        # The message has to be actionable without reading the code: the fix for
        # an over-budget projection requires a different caller-supplied limit.
        with self.assertRaises(BudgetExceeded) as raised:
            CostGuard(ceiling_usd=10.0).check_projection(100)
        message = str(raised.exception)
        self.assertIn("84.00", message)
        self.assertIn("10.00", message)
        self.assertIn("--api-cost-ceiling-usd", message)

    def test_the_refusal_is_a_boundary_not_a_rounding(self) -> None:
        # Exactly at the ceiling is allowed; one episode more is not. A guard
        # that only fired far past the ceiling would still overspend.
        ceiling = 10 * PLANNING_COST_PER_EPISODE_USD
        CostGuard(ceiling_usd=ceiling).check_projection(10)
        with self.assertRaises(BudgetExceeded):
            CostGuard(ceiling_usd=ceiling).check_projection(11)

    def test_a_negative_episode_count_is_a_programming_error(self) -> None:
        with self.assertRaises(ValueError):
            CostGuard(ceiling_usd=1.0).check_projection(-1)


class ProjectionByModeChargesEachModeItsOwnRate(unittest.TestCase):
    """``check_projection_by_mode`` — for a sweep whose modes cost differently."""

    def test_a_mode_missing_from_the_rate_map_falls_back_to_the_flat_rate(self) -> None:
        guard = CostGuard(ceiling_usd=100.0)
        projected = guard.check_projection_by_mode({"unpriced": 5}, {})
        self.assertAlmostEqual(projected, 5 * PLANNING_COST_PER_EPISODE_USD, places=6)

    def test_each_modes_own_rate_is_summed_separately(self) -> None:
        guard = CostGuard(ceiling_usd=100.0)
        projected = guard.check_projection_by_mode(
            {"cheap": 10, "expensive": 10}, {"cheap": 0.10, "expensive": 1.00}
        )
        self.assertAlmostEqual(projected, 10 * 0.10 + 10 * 1.00, places=6)

    def test_a_projection_that_only_clears_the_flat_rate_is_still_refused(self) -> None:
        # Ten episodes at the flat rate would fit an $8.40 ceiling; a rate map
        # that charges one mode double must still refuse before anything runs.
        guard = CostGuard(ceiling_usd=10 * PLANNING_COST_PER_EPISODE_USD)
        with self.assertRaises(BudgetExceeded):
            guard.check_projection_by_mode(
                {"heavy": 10}, {"heavy": 2 * PLANNING_COST_PER_EPISODE_USD}
            )

    def test_a_negative_episode_count_is_a_programming_error(self) -> None:
        with self.assertRaises(ValueError):
            CostGuard(ceiling_usd=1.0).check_projection_by_mode({"mode": -1}, {})


class NullCostIsUnknownNotFree(unittest.TestCase):
    """The sharp edge the usage contract documents."""

    def test_an_unpriced_model_family_halts_instead_of_spending_nothing(self) -> None:
        guard = CostGuard(ceiling_usd=100.0)
        guard.record(usage(None))
        # Read as zero this would have been a free episode and the sweep would
        # have run the whole matrix against an unbounded true cost.
        self.assertEqual(guard.spent_usd, 0.0)
        self.assertEqual(guard.unknown_cost_episodes, 1)
        self.assertFalse(guard.cost_is_complete)
        self.assertFalse(guard.may_schedule())
        self.assertEqual(guard.halt_reason, HALT_UNKNOWN_COST)

    def test_an_unmeasured_episode_halts_for_the_same_reason(self) -> None:
        guard = CostGuard(ceiling_usd=100.0)
        guard.record(None)
        self.assertFalse(guard.may_schedule())
        self.assertEqual(guard.halt_reason, HALT_UNKNOWN_COST)

    def test_an_episode_that_never_launched_the_agent_is_a_known_zero(self) -> None:
        # A container that would not start could not reach a provider, so its
        # cost is known rather than unknown. Collapsing the two would let one
        # image-pull failure abandon a paid sweep.
        guard = CostGuard(ceiling_usd=100.0)
        guard.record(None, billable=False)
        self.assertTrue(guard.may_schedule())
        self.assertEqual(guard.unknown_cost_episodes, 0)
        self.assertTrue(guard.cost_is_complete)
        self.assertIsNone(guard.halt_reason)


class MeasuredSpendStopsScheduling(unittest.TestCase):
    """``may_schedule`` closes the gate once the ceiling is reached."""

    def test_scheduling_stops_at_the_ceiling_and_keeps_what_was_spent(self) -> None:
        guard = CostGuard(ceiling_usd=1.0)
        for _ in range(4):
            if not guard.may_schedule():
                break
            guard.record(usage(0.3))
        self.assertEqual(guard.halt_reason, HALT_CEILING)
        self.assertAlmostEqual(guard.spent_usd, 1.2, places=6)
        # Four episodes were planned and the guard stopped after the fourth
        # crossed the line, rather than after all of them.
        self.assertFalse(guard.may_schedule())

    def test_spend_below_the_ceiling_never_halts(self) -> None:
        guard = CostGuard(ceiling_usd=10.0)
        for _ in range(5):
            guard.record(usage(0.5))
        self.assertTrue(guard.may_schedule())
        self.assertIsNone(guard.halt_reason)
        self.assertAlmostEqual(guard.spent_usd, 2.5, places=6)

    def test_the_first_halt_reason_is_the_one_reported(self) -> None:
        # An unknown cost followed by a ceiling breach still reports the
        # unknown, because that is the fact that stopped the run.
        guard = CostGuard(ceiling_usd=1.0)
        guard.record(usage(None))
        guard.record(usage(5.0))
        self.assertEqual(guard.halt_reason, HALT_UNKNOWN_COST)

    def test_the_summary_projection_reports_every_field_a_reader_needs(self) -> None:
        guard = CostGuard(ceiling_usd=2.0)
        guard.record(usage(0.25))
        guard.record(usage(None))
        self.assertEqual(
            guard.to_json(),
            {
                "ceiling_usd": 2.0,
                "planning_cost_per_episode_usd": PLANNING_COST_PER_EPISODE_USD,
                "measured_cost_usd": 0.25,
                "unknown_cost_episodes": 1,
                "cost_is_complete": False,
                "halt_reason": HALT_UNKNOWN_COST,
            },
        )


class ChooserCapMirrorsTheHost(unittest.TestCase):
    """The hand-synced chooser-call cap must match the host's Rust constant."""

    def test_python_cap_equals_the_rust_constant(self) -> None:
        import re
        from pathlib import Path

        from ..config import MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN

        source = (
            Path(__file__).resolve().parents[3]
            / "crates/polaris_code_core/src/cosmos/composition.rs"
        ).read_text(encoding="utf-8")
        found = re.search(
            r"pub const MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN: usize = (\d+);", source
        )
        self.assertIsNotNone(found, "the Rust constant moved or was renamed")
        self.assertEqual(int(found.group(1)), MAX_MODEL_CHOOSER_ATTEMPTS_PER_TURN)


if __name__ == "__main__":
    unittest.main()
