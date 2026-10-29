"""``TokenMeter`` passthrough: read the run's accounting, never recompute it.

The adapter consumes the run's accounting instead of recomputing token pricing,
because two accounting authorities cannot both be right. These tests hold it
to the contract in ``docs/headless-usage-accounting.md``
(``polaris-code-run-usage/v1``), including the two null arms that a careless
reader would turn into a zero.

The object in :data:`CONTRACT_OBJECT` mirrors the example in that document,
float tail and all, so that a change in the agent's output shape fails here
rather than silently under-reporting a sweep's cost.
"""

from __future__ import annotations

import json
import unittest

from ..usage import RUN_USAGE_SCHEMA, RunResult, RunUsage, UsageError
from ..episode import outcome

#: The accounting object as the documented contract shows it.
CONTRACT_OBJECT = {
    "schema": "polaris-code-run-usage/v1",
    "model_id": "anthropic/claude-sonnet-4-6",
    "input_tokens": 1234,
    "output_tokens": 56,
    "cache_read_tokens": 789,
    "cache_creation_tokens": 12,
    "total_tokens": 2091,
    "estimated_cost_usd": 0.004823700000000001,
    "pricing_basis": "recorded-price-table",
    "pricing": {
        "family": "sonnet",
        "input_per_mtok": 3.0,
        "output_per_mtok": 15.0,
        "cache_read_per_mtok": 0.3,
        "cache_write_per_mtok": 3.75,
    },
}


def result_object(**overrides: object) -> dict:
    """A whole ``--output-format json`` object with ``usage`` filled in."""
    body = {
        "session_id": "eval-curated-1-aaa__one-1",
        "turn_number": 1,
        "run_id": "run-1",
        "status": "completed",
        "result": "done",
        "error": None,
        "usage": dict(CONTRACT_OBJECT),
        "max_output_tokens": 4096,
    }
    body.update(overrides)
    return body


class TheContractObjectIsReadFieldForField(unittest.TestCase):
    """Every counter arrives unchanged, and so does the rate card."""

    def test_episode_projects_the_model_count_separately_from_user_turns(self) -> None:
        for count in (None, 0, 100):
            usage = RunUsage.parse(dict(CONTRACT_OBJECT, num_turns=count))
            episode = outcome(
                instance_id="fixture", mode="curated", repetition=1, attempt=1,
                session="fixture", termination="completed", exit_code=0,
                turn_number=1, run_id="fixture", status="completed", usage=usage,
                max_output_tokens=4096, patch="", image="fixture", image_digest=None,
                base_commit="fixture", head_commit=None, tree_was_clean=True,
                error=None, seconds=0.0, commands=(), arm="lean",
            ).to_json("fixture-schema")
            self.assertEqual(episode["num_turns"], count)
            self.assertEqual(episode["turn_number"], 1)

    def test_num_turns_is_preserved_and_old_records_remain_unknown(self) -> None:
        self.assertIsNone(RunUsage.parse(dict(CONTRACT_OBJECT)).num_turns)
        measured = RunUsage.parse(dict(CONTRACT_OBJECT, num_turns=100))
        self.assertEqual(measured.num_turns, 100)
        self.assertEqual(measured.to_json()["num_turns"], 100)
        for value in (-1, True, "3", 2.5):
            with self.assertRaises(UsageError):
                RunUsage.parse(dict(CONTRACT_OBJECT, num_turns=value))

    def test_the_published_object_parses_to_its_own_values(self) -> None:
        usage = RunUsage.parse(CONTRACT_OBJECT)
        self.assertEqual(usage.model_id, "anthropic/claude-sonnet-4-6")
        self.assertEqual(usage.input_tokens, 1234)
        self.assertEqual(usage.output_tokens, 56)
        self.assertEqual(usage.cache_read_tokens, 789)
        self.assertEqual(usage.cache_creation_tokens, 12)
        self.assertEqual(usage.total_tokens, 2091)
        self.assertEqual(usage.pricing_basis, "recorded-price-table")
        self.assertTrue(usage.cost_is_known)

    def test_the_float_tail_survives_the_round_trip(self) -> None:
        # The contract warns that the cost is a sum of four f64 products and
        # carries binary residue. Rounding it here would make the adapter's
        # total disagree with the producer's own number.
        usage = RunUsage.parse(CONTRACT_OBJECT)
        self.assertEqual(usage.estimated_cost_usd, 0.004823700000000001)
        self.assertEqual(usage.to_json()["estimated_cost_usd"], 0.004823700000000001)

    def test_the_episode_projection_is_the_source_object(self) -> None:
        # Field-for-field equality is the passthrough property: an episode
        # record carries what the run reported, not a re-derivation of it.
        self.assertEqual(RunUsage.parse(CONTRACT_OBJECT).to_json(), CONTRACT_OBJECT)

    def test_the_rate_card_is_stored_verbatim_and_never_corrected(self) -> None:
        # The adapter has no price table of its own, so a rate card it does not
        # recognise must still round-trip untouched. If this adapter ever grew
        # an opinion about rates, this is where it would show.
        odd = dict(CONTRACT_OBJECT)
        odd["pricing"] = {"family": "invented", "input_per_mtok": 99.5}
        odd["pricing_basis"] = "some-other-table-2099-01"
        parsed = RunUsage.parse(odd)
        self.assertEqual(parsed.pricing, {"family": "invented", "input_per_mtok": 99.5})
        self.assertEqual(parsed.pricing_basis, "some-other-table-2099-01")
        self.assertEqual(parsed.to_json()["pricing"], odd["pricing"])

    def test_the_cost_is_not_re_derived_from_the_counters(self) -> None:
        # Pricing the four counters at the emitted rates gives ~0.0048237. The
        # adapter reports whatever the run said even when the two disagree,
        # because the run's meter is the authority and a silent correction
        # would hide a producer bug rather than surface it.
        disagreeing = dict(CONTRACT_OBJECT)
        disagreeing["estimated_cost_usd"] = 42.0
        self.assertEqual(RunUsage.parse(disagreeing).estimated_cost_usd, 42.0)


class NullMeansUnknownAndNeverZero(unittest.TestCase):
    """The sharp edge the usage contract documents, at the parse boundary."""

    def test_an_unpriced_family_reports_an_unknown_cost(self) -> None:
        unpriced = dict(CONTRACT_OBJECT)
        unpriced["estimated_cost_usd"] = None
        unpriced["pricing"] = None
        usage = RunUsage.parse(unpriced)
        self.assertIsNone(usage.estimated_cost_usd)
        self.assertNotEqual(usage.estimated_cost_usd, 0.0)
        self.assertFalse(usage.cost_is_known)

    def test_an_unpriced_run_still_names_the_table_it_was_attributed_to(self) -> None:
        # The contract guarantees pricing_basis is always present, including
        # when pricing is null, so an unpriced run stays attributable.
        unpriced = dict(CONTRACT_OBJECT)
        unpriced["estimated_cost_usd"] = None
        unpriced["pricing"] = None
        self.assertEqual(RunUsage.parse(unpriced).pricing_basis, "recorded-price-table")

    def test_an_unmeasured_run_reports_no_accounting_at_all(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object(usage=None)))
        self.assertIsNone(parsed.usage)
        self.assertEqual(parsed.status, "completed")


class AnUnreadableRecordIsRefusedRatherThanCoerced(unittest.TestCase):
    """The version field only earns its keep if an unknown value is fatal."""

    def test_an_unrecognised_schema_is_refused(self) -> None:
        moved = dict(CONTRACT_OBJECT)
        moved["schema"] = "polaris-code-run-usage/v2"
        with self.assertRaises(UsageError) as raised:
            RunUsage.parse(moved)
        self.assertIn("polaris-code-run-usage/v2", str(raised.exception))
        self.assertIn(RUN_USAGE_SCHEMA, str(raised.exception))

    def test_a_missing_schema_is_refused(self) -> None:
        without = {k: v for k, v in CONTRACT_OBJECT.items() if k != "schema"}
        with self.assertRaises(UsageError):
            RunUsage.parse(without)

    def test_a_missing_counter_names_the_field(self) -> None:
        for field in (
            "input_tokens", "output_tokens", "cache_read_tokens",
            "cache_creation_tokens", "total_tokens", "pricing_basis", "model_id",
        ):
            with self.subTest(field=field):
                partial = {k: v for k, v in CONTRACT_OBJECT.items() if k != field}
                with self.assertRaises(UsageError):
                    RunUsage.parse(partial)

    def test_a_total_that_is_not_the_sum_of_its_tiers_is_refused(self) -> None:
        # The agent guarantees this identity, so a violation means the record
        # was edited or came from a different source. Either way collating it
        # would report a number nobody measured.
        tampered = dict(CONTRACT_OBJECT)
        tampered["total_tokens"] = 99
        with self.assertRaises(UsageError) as raised:
            RunUsage.parse(tampered)
        self.assertIn("2091", str(raised.exception))

    def test_stdout_that_is_not_one_json_object_is_refused(self) -> None:
        for stdout in ("", "   ", "not json at all", "[1, 2, 3]", '"a string"'):
            with self.subTest(stdout=stdout):
                with self.assertRaises(UsageError):
                    RunResult.parse(stdout)

    def test_a_result_whose_usage_is_unreadable_fails_the_whole_result(self) -> None:
        # Refusing the result rather than the accounting alone is deliberate:
        # an episode with an unreadable cost must not be collated as if it were
        # free, and `unreadable-result` is already a termination this adapter
        # records.
        broken = result_object()
        broken["usage"] = {**CONTRACT_OBJECT, "schema": "nope/v1"}
        with self.assertRaises(UsageError):
            RunResult.parse(json.dumps(broken))


class TheIdentityTripleSurvivesTheParse(unittest.TestCase):
    """An episode record has to be traceable back to the run that produced it."""

    def test_session_turn_and_run_id_are_read_from_the_same_object(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object()))
        self.assertEqual(parsed.session_id, "eval-curated-1-aaa__one-1")
        self.assertEqual(parsed.turn_number, 1)
        self.assertEqual(parsed.run_id, "run-1")

    def test_a_failed_turn_keeps_its_error_and_its_accounting(self) -> None:
        # An interrupted or failed run still reports what it spent before
        # stopping, and that spend counts against the ceiling.
        failed = result_object(status="failed", result=None, error="tool exploded")
        parsed = RunResult.parse(json.dumps(failed))
        self.assertEqual(parsed.status, "failed")
        self.assertEqual(parsed.error, "tool exploded")
        self.assertIsNotNone(parsed.usage)
        self.assertEqual(parsed.usage.total_tokens, 2091)  # type: ignore[union-attr]

    def test_an_absent_run_id_is_none_rather_than_the_string_none(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object(run_id=None)))
        self.assertIsNone(parsed.run_id)


class TheOutputTokenLimitIsCarriedThrough(unittest.TestCase):
    """A benchmark number has to be able to state the bound it ran under."""

    def test_the_reported_limit_is_read_as_an_int(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object(max_output_tokens=32000)))
        self.assertEqual(parsed.max_output_tokens, 32000)

    def test_an_agent_that_does_not_report_a_limit_gives_none(self) -> None:
        body = result_object()
        del body["max_output_tokens"]
        parsed = RunResult.parse(json.dumps(body))
        self.assertIsNone(
            parsed.max_output_tokens,
            "an absent limit is UNKNOWN, not unbounded and not zero",
        )


class TheAssembledCompositionIsReadBackFromTheReport(unittest.TestCase):
    """The whole point of the composition axis: read what was assembled, not
    only what was requested."""

    def test_a_runtime_assembled_composition_is_read_field_for_field(self) -> None:
        body = result_object(topology={
            "composition": {
                "policy": "model",
                "recipe": "direct-verified@v1",
                "digest": "sha256:" + "b" * 64,
                "forced": False,
            }
        })
        parsed = RunResult.parse(json.dumps(body))
        self.assertEqual(parsed.composition_policy, "model")
        self.assertEqual(parsed.composition_recipe, "direct-verified@v1")
        self.assertEqual(parsed.composition_digest, "sha256:" + "b" * 64)
        self.assertEqual(parsed.composition_forced, False)

    def test_a_conversational_turn_carries_no_composition_identity(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object()))
        self.assertIsNone(parsed.composition_policy)
        self.assertIsNone(parsed.composition_recipe)
        self.assertIsNone(parsed.composition_digest)
        self.assertIsNone(parsed.composition_forced)

    def test_a_topology_answer_without_a_composition_carries_no_composition_identity(self) -> None:
        # A turn that started a topology whose result did not reach the
        # runtime-assembly path (e.g. the non-mutating tree-question shape)
        # reports `topology` without a `composition` member at all.
        body = result_object(topology={"registration_id": "code.tree-question"})
        parsed = RunResult.parse(json.dumps(body))
        self.assertIsNone(parsed.composition_policy)

    def test_an_explicitly_forced_composition_is_read_as_forced(self) -> None:
        body = result_object(topology={
            "composition": {
                "policy": "deterministic",
                "recipe": "direct-verified@v1",
                "digest": "sha256:" + "c" * 64,
                "forced": True,
            }
        })
        parsed = RunResult.parse(json.dumps(body))
        self.assertTrue(parsed.composition_forced)

    def test_gate_plan_evidence_is_read_without_inferring_it_from_the_request(self) -> None:
        body = result_object(topology={
            "gate_plan": {
                "plan_digest": "sha256:" + "a" * 64,
                "shape_digest": "sha256:" + "b" * 64,
                "shape": "Seq(model-propose, apply-patch-intent)",
                "planner": "polaris-code.gate-planner-model",
                "revision_attempt": 2,
                "prior_refused_attempts": 1,
            },
            "exhausted_gate_plan": {"refused_attempts": 3},
        })
        parsed = RunResult.parse(json.dumps(body))
        self.assertEqual(parsed.gate_plan_digest, "sha256:" + "a" * 64)
        self.assertEqual(parsed.gate_plan_shape_digest, "sha256:" + "b" * 64)
        self.assertEqual(parsed.gate_plan_shape, "Seq(model-propose, apply-patch-intent)")
        self.assertEqual(parsed.gate_plan_planner, "polaris-code.gate-planner-model")
        self.assertEqual(parsed.gate_plan_revision_attempt, 2)
        self.assertEqual(parsed.gate_plan_prior_refused_attempts, 1)
        self.assertEqual(parsed.exhausted_gate_plan_refused_attempts, 3)

    def test_an_old_report_without_gate_plan_evidence_preserves_unknown(self) -> None:
        parsed = RunResult.parse(json.dumps(result_object()))
        self.assertIsNone(parsed.gate_plan_digest)
        self.assertIsNone(parsed.gate_plan_shape_digest)
        self.assertIsNone(parsed.gate_plan_shape)
        self.assertIsNone(parsed.gate_plan_planner)
        self.assertIsNone(parsed.gate_plan_revision_attempt)
        self.assertIsNone(parsed.gate_plan_prior_refused_attempts)
        self.assertIsNone(parsed.exhausted_gate_plan_refused_attempts)


if __name__ == "__main__":
    unittest.main()
