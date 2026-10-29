"""The sweep: immutable directories, an enforced ceiling, a reproducible summary.

These drive the whole orchestrator through
:class:`~evals.core.process.ReplayRunner`, so what is under
test is the real control flow of a measured sweep — the same argv, the same
records, the same collation — with only the kernel call substituted. Nothing
here opens a socket, reads a provider key or starts a container.
"""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from ....core.budget import BudgetExceeded, HALT_CEILING, HALT_UNKNOWN_COST
from ....core.collate import CONCLUSION_PLUMBING
from ....core.config import (
    AXIS_COMPOSITION,
    AXIS_TOPOLOGY,
    COMPOSITION_POLICY_PLANNING_COST_PER_EPISODE_USD,
    MODES,
    PLANNING_COST_PER_EPISODE_USD,
)
from ....core.episode import TERMINATION_COMPLETED, TERMINATION_INFRASTRUCTURE
from ....core.process import ReplayRunner
from ....core.runner import DirtyTree, RunDirectoryExists, plan_order, run_sweep
from ..grader import grader_run_id
from ..pack import SwebenchMultilingualPack
from ..replay_script import ScriptedEpisode, build
from .support import (
    TINY_IDS,
    config,
    issued_lines,
    pack_recompute,
    read_jsonl,
    replay_sweep,
    scripted_sweep,
    tiny_suite,
)


class Sweeping(unittest.TestCase):
    """Shared scaffolding: a temporary root and a synthetic suite."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.suite = tiny_suite()

    def sweep(self, *, name: str = "sbml-demo", **kwargs):
        """Run one replayed sweep into a fresh directory under the root."""
        output = self.root / name
        run_config = config(output=output, **{k: v for k, v in kwargs.items() if k in {
            "modes", "repetitions", "instance_ids", "ceiling", "binary", "allow_edit", "allow_command",
            "topology_work", "axis", "graph_search_mode", "composition_policy",
        }})
        episodes = kwargs.get("episodes")
        if episodes is None:
            episodes = scripted_sweep(
                suite=self.suite,
                eval_run_id=output.name,
                modes=run_config.modes,
                repetitions=run_config.repetitions,
                resolved=kwargs.get("resolved", ()),
                cost_per_episode=kwargs.get("cost_per_episode", 0.01),
            )
        return replay_sweep(tmp=output, suite=self.suite, episodes=episodes, run_config=run_config)


class ARunDirectoryIsImmutable(Sweeping):
    """Evidence is written once. Reusing a directory would overwrite it."""

    def test_a_non_empty_output_directory_is_refused(self) -> None:
        result, _ = self.sweep()
        with self.assertRaises(RunDirectoryExists) as raised:
            self.sweep()
        self.assertIn("immutable", str(raised.exception))
        # The first run's evidence is untouched by the refusal.
        self.assertTrue((result.run_dir / "summary.json").is_file())

    def test_an_empty_directory_may_be_used(self) -> None:
        (self.root / "prepared").mkdir()
        result, _ = self.sweep(name="prepared")
        self.assertTrue((result.run_dir / "manifest.json").is_file())

    def test_the_expected_files_are_all_written(self) -> None:
        result, _ = self.sweep()
        for relative in (
            "manifest.json", "instance_ids.txt", "episodes.jsonl",
            "predictions.jsonl", "grader/results.json", "summary.json",
        ):
            with self.subTest(relative=relative):
                self.assertTrue((result.run_dir / relative).is_file(), relative)

    def test_instance_ids_lists_the_whole_pinned_suite_not_the_subset(self) -> None:
        # The denominator is the pinned list, so the file that records it must
        # not shrink when a run attempts fewer.
        result, _ = self.sweep(instance_ids=[TINY_IDS[0]])
        listed = (result.run_dir / "instance_ids.txt").read_text(encoding="utf-8").split()
        self.assertEqual(tuple(listed), self.suite.instance_ids)

    def test_every_episode_gets_a_patch_file_even_when_empty(self) -> None:
        result, _ = self.sweep(instance_ids=[TINY_IDS[0]], modes=["curated"])
        patch = result.run_dir / "logs" / "curated" / "1" / TINY_IDS[0] / "attempt-1.patch"
        self.assertTrue(patch.is_file())
        self.assertEqual(patch.read_text(encoding="utf-8"), "")

    def test_a_measured_run_from_a_dirty_checkout_is_refused(self) -> None:
        output = self.root / "dirty"
        run_config = config(output=output)
        runner = ReplayRunner([])
        with mock.patch(
            "evals.core.runner.repository_state",
            return_value={"path": "polaris_code", "commit": "abc123", "clean": False},
        ):
            with self.assertRaises(DirtyTree) as raised:
                run_sweep(
                    config=run_config,
                    pack=SwebenchMultilingualPack(),
                    suite=self.suite,
                    output=output,
                    process_runner=runner,
                    command_line=["run"],
                    repositories=[self.root / "polaris_code"],
                    require_clean_tree=True,
                    now=lambda: "2000-01-01T00:00:00Z",
                )
        self.assertIn("dirty", str(raised.exception))
        self.assertEqual(runner.issued, [])
        self.assertFalse(output.exists())


class TheCostCeilingCannotBeSpentByAccident(Sweeping):
    """Refused before starting, and closed once the measured spend arrives."""

    def test_an_unaffordable_projection_refuses_before_anything_runs(self) -> None:
        output = self.root / "too-big"
        run_config = config(output=output, ceiling=PLANNING_COST_PER_EPISODE_USD)
        runner = ReplayRunner(build(suite=self.suite, eval_run_id=output.name, scripted=[]))
        with self.assertRaises(BudgetExceeded):
            run_sweep(
                config=run_config, pack=SwebenchMultilingualPack(), suite=self.suite,
                output=output, process_runner=runner,
                command_line=["run"], require_clean_tree=False,
                now=lambda: "2000-01-01T00:00:00Z",
            )
        # Nothing was issued and no directory was populated: the refusal comes
        # before the first container, not at episode 300.
        self.assertEqual(runner.issued, [])
        self.assertFalse((output / "manifest.json").exists())

    def test_reaching_the_ceiling_stops_scheduling_and_withholds_the_score(self) -> None:
        # The projection ($7.56) is affordable, but measured spend is not:
        # three episodes at $3.00 cross an $8.00 ceiling.
        result, runner = self.sweep(ceiling=8.0, cost_per_episode=3.0)
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual(len(records), 3)
        self.assertEqual(result.manifest["budget"]["halt_reason"], HALT_CEILING)
        self.assertAlmostEqual(result.manifest["budget"]["measured_cost_usd"], 9.0, places=6)
        self.assertFalse(result.summary["complete"])
        for cell in result.summary["cells"]:
            self.assertIsNone(cell["resolved_rate"])

    def test_an_unpriced_episode_halts_the_sweep_rather_than_running_free(self) -> None:
        # The sharp edge, end to end: a null cost must not read as zero and let
        # the whole matrix run against an unbounded true spend.
        result, _ = self.sweep(cost_per_episode=None)
        self.assertEqual(result.manifest["budget"]["halt_reason"], HALT_UNKNOWN_COST)
        self.assertEqual(len(read_jsonl(result.run_dir / "episodes.jsonl")), 1)
        self.assertFalse(result.manifest["budget"]["cost_is_complete"])

    def test_a_sweep_within_budget_runs_every_cell(self) -> None:
        result, _ = self.sweep()
        self.assertEqual(len(read_jsonl(result.run_dir / "episodes.jsonl")), 9)
        self.assertIsNone(result.manifest["budget"]["halt_reason"])

    def test_the_projection_is_recorded_in_the_manifest(self) -> None:
        result, _ = self.sweep()
        self.assertEqual(result.manifest["episodes_planned"], 9)
        self.assertAlmostEqual(
            result.manifest["projected_cost_usd"], 9 * PLANNING_COST_PER_EPISODE_USD, places=6
        )

    def test_an_unaffordable_topology_matrix_refuses_before_any_episode(self) -> None:
        output = self.root / "topology-too-big"
        run_config = config(
            output=output,
            modes=("code.direct-edit", "code.tree-question"),
            ceiling=PLANNING_COST_PER_EPISODE_USD,
            topology_work=True,
            axis=AXIS_TOPOLOGY,
            graph_search_mode="both",
        )
        runner = ReplayRunner([])
        with self.assertRaises(BudgetExceeded):
            run_sweep(
                config=run_config,
                pack=SwebenchMultilingualPack(),
                suite=self.suite,
                output=output,
                process_runner=runner,
                command_line=["run"],
                require_clean_tree=False,
                now=lambda: "2000-01-01T00:00:00Z",
            )
        self.assertEqual(runner.issued, [])
        self.assertFalse((output / "manifest.json").exists())


class TheScheduleIsAPureFunctionOfTheRecordedConfiguration(Sweeping):
    """Two runs of one manifest attempt the same episodes in the same order."""

    def test_the_same_seed_gives_the_same_order(self) -> None:
        first = plan_order(instances=self.suite.instances, modes=MODES, repetitions=2, order_seed=7)
        second = plan_order(instances=self.suite.instances, modes=MODES, repetitions=2, order_seed=7)
        self.assertEqual(
            [(i.instance_id, m, r) for i, m, r in first],
            [(i.instance_id, m, r) for i, m, r in second],
        )

    def test_the_seed_actually_shuffles_the_modes(self) -> None:
        canonical = [m for _, m, _ in plan_order(
            instances=self.suite.instances, modes=MODES, repetitions=1, order_seed=7
        )]
        self.assertNotEqual(canonical, list(MODES) * len(self.suite.instances))

    def test_every_cell_is_scheduled_exactly_once(self) -> None:
        schedule = plan_order(instances=self.suite.instances, modes=MODES, repetitions=3, order_seed=1)
        keys = [(i.instance_id, m, r) for i, m, r in schedule]
        self.assertEqual(len(keys), len(set(keys)))
        self.assertEqual(len(keys), len(self.suite.instances) * len(MODES) * 3)


class TopologyIsAnExperimentAxis(Sweeping):
    """Topology cells use the established runner, grader and evidence path."""

    def test_replay_records_each_topology_cell_with_its_own_identity_and_axis(self) -> None:
        topologies = ("code.direct-edit", "code.tree-question")
        output = self.root / "topology-axis"
        run_config = config(
            output=output,
            modes=topologies,
            topology_work=True,
            axis=AXIS_TOPOLOGY,
            graph_search_mode="both",
        )
        episodes = scripted_sweep(
            suite=self.suite,
            eval_run_id=output.name,
            modes=topologies,
        )
        result, runner = replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=episodes,
            run_config=run_config,
        )
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual({record["mode"] for record in records}, set(topologies))
        self.assertEqual(len({record["session_id"] for record in records}), len(records))
        created = [line for line in issued_lines(runner) if " create --name " in line]
        self.assertEqual(len(created), len(records))
        self.assertEqual(len(set(created)), len(created))
        self.assertEqual(result.manifest["config"]["axis"], AXIS_TOPOLOGY)
        self.assertEqual(result.manifest["config"]["graph_search_mode"], "both")
        self.assertEqual(result.manifest["episodes_planned"], len(self.suite.instances) * len(topologies))
        self.assertEqual(result.summary["axis"], AXIS_TOPOLOGY)
        self.assertEqual(result.summary["conclusion"], CONCLUSION_PLUMBING)
        turn_lines = [line for line in issued_lines(runner) if "--output-format json" in line]
        for topology in topologies:
            self.assertTrue(any(f"POLARIS_CODE_TOPOLOGY_PIN={topology}" in line for line in turn_lines))

    def test_topology_cells_remain_incomplete_against_the_pinned_denominator(self) -> None:
        result, _ = self.sweep(
            modes=("code.direct-edit", "code.tree-question"),
            topology_work=True,
            axis=AXIS_TOPOLOGY,
            graph_search_mode="both",
        )
        self.assertEqual(result.manifest["config"]["axis"], AXIS_TOPOLOGY)
        self.assertTrue(all(cell["denominator"] == 43 for cell in result.summary["cells"]))
        self.assertTrue(all(cell["resolved_rate"] is None for cell in result.summary["cells"]))


class CompositionIsAnExperimentAxis(Sweeping):
    """Composition cells use the established runner, grader and evidence path."""

    def test_replay_records_each_composition_cell_with_its_own_identity_and_axis(self) -> None:
        policies = ("deterministic", "model")
        output = self.root / "composition-axis"
        run_config = config(
            output=output,
            modes=policies,
            topology_work=True,
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
        )
        episodes = scripted_sweep(
            suite=self.suite,
            eval_run_id=output.name,
            modes=policies,
        )
        result, runner = replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=episodes,
            run_config=run_config,
        )
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual({record["mode"] for record in records}, set(policies))
        self.assertEqual(len({record["session_id"] for record in records}), len(records))
        created = [line for line in issued_lines(runner) if " create --name " in line]
        self.assertEqual(len(created), len(records))
        self.assertEqual(len(set(created)), len(created))
        self.assertEqual(result.manifest["config"]["axis"], AXIS_COMPOSITION)
        self.assertEqual(result.manifest["config"]["graph_search_mode"], "both")
        self.assertEqual(result.manifest["episodes_planned"], len(self.suite.instances) * len(policies))
        self.assertEqual(result.summary["axis"], AXIS_COMPOSITION)
        self.assertEqual(result.summary["conclusion"], CONCLUSION_PLUMBING)
        turn_lines = [line for line in issued_lines(runner) if "--output-format json" in line]
        for policy in policies:
            self.assertTrue(
                any(f"POLARIS_CODE_COMPOSITION_POLICY={policy}" in line for line in turn_lines)
            )
        # Graph search stays fixed and topology stays unpinned: only the
        # composition policy is meant to vary between these cells.
        self.assertTrue(all("POLARIS_CODE_GRAPH_SEARCH=both" in line for line in turn_lines))
        self.assertTrue(all("POLARIS_CODE_TOPOLOGY_PIN" not in line for line in turn_lines))
        self.assertTrue(all("topology=code.tree-question" not in line for line in turn_lines))

    def test_composition_cells_remain_incomplete_against_the_pinned_denominator(self) -> None:
        result, _ = self.sweep(
            modes=("deterministic", "model"),
            topology_work=True,
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
        )
        self.assertEqual(result.manifest["config"]["axis"], AXIS_COMPOSITION)
        self.assertTrue(all(cell["denominator"] == 43 for cell in result.summary["cells"]))
        self.assertTrue(all(cell["resolved_rate"] is None for cell in result.summary["cells"]))

    def test_the_projection_charges_the_model_cell_more_than_the_flat_rate(self) -> None:
        # Three instances, two cells. A flat per-episode rate would let this
        # sweep start under a ceiling set between the flat and the weighted
        # projection. The `model` cell's own bounded chooser-call ceiling
        # makes the true projection higher, so such a run must still be
        # refused before any episode runs.
        flat_projection = len(self.suite.instances) * len(("deterministic", "model")) * (
            PLANNING_COST_PER_EPISODE_USD
        )
        weighted_projection = len(self.suite.instances) * sum(
            COMPOSITION_POLICY_PLANNING_COST_PER_EPISODE_USD[policy]
            for policy in ("deterministic", "model")
        )
        ceiling = (flat_projection + weighted_projection) / 2
        self.assertGreater(
            weighted_projection, ceiling, "the chosen ceiling must sit strictly below the real cost"
        )
        self.assertLess(
            flat_projection, ceiling, "the chosen ceiling must sit strictly above the flat projection"
        )
        output = self.root / "composition-model-too-big"
        run_config = config(
            output=output,
            modes=("deterministic", "model"),
            ceiling=ceiling,
            topology_work=True,
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
        )
        runner = ReplayRunner([])
        with self.assertRaises(BudgetExceeded):
            run_sweep(
                config=run_config,
                pack=SwebenchMultilingualPack(),
                suite=self.suite,
                output=output,
                process_runner=runner,
                command_line=["run"],
                require_clean_tree=False,
                now=lambda: "2000-01-01T00:00:00Z",
            )
        self.assertEqual(runner.issued, [])
        self.assertFalse((output / "manifest.json").exists())

    def test_the_composition_the_binary_actually_assembled_is_recorded_per_episode(self) -> None:
        # The axis records what the run's own report says was assembled, not
        # only what the episode requested — a cell asking for "model" must not
        # be trusted to have received a model-chosen recipe just because that
        # is what it asked for.
        policies = ("deterministic", "model")
        recipe_by_policy = {"deterministic": "direct-verified@v1", "model": "prediction-error@v1"}
        digest_by_policy = {"deterministic": "sha256:" + "d" * 64, "model": "sha256:" + "5" * 64}
        output = self.root / "composition-identity"
        run_config = config(
            output=output,
            modes=policies,
            topology_work=True,
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
        )
        episodes = [
            ScriptedEpisode(
                instance_id=instance.instance_id,
                mode=policy,
                composition_policy=policy,
                composition_recipe=recipe_by_policy[policy],
                composition_digest=digest_by_policy[policy],
                composition_forced=False,
            )
            for policy in policies
            for instance in self.suite.instances
        ]
        result, _ = replay_sweep(
            tmp=output, suite=self.suite, episodes=episodes, run_config=run_config,
        )
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual(len(records), len(self.suite.instances) * len(policies))
        for record in records:
            self.assertEqual(record["composition_policy"], record["mode"])
            self.assertEqual(record["composition_recipe"], recipe_by_policy[record["mode"]])
            self.assertEqual(record["composition_digest"], digest_by_policy[record["mode"]])
            self.assertFalse(record["composition_forced"])


class InfrastructureFailuresAreRetriedAndResultsAreNot(Sweeping):
    """Only a confirmed infrastructure failure gets a second attempt."""

    def test_a_container_that_will_not_come_up_is_retried_once_and_both_kept(self) -> None:
        episodes = [
            ScriptedEpisode(
                instance_id=TINY_IDS[0], mode="curated",
                container_failure="no such image",
            )
        ]
        result, _ = self.sweep(
            instance_ids=[TINY_IDS[0]], modes=["curated"], episodes=episodes,
        )
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual(len(records), 2)
        self.assertEqual([r["attempt"] for r in records], [1, 2])
        for record in records:
            self.assertEqual(record["termination"], TERMINATION_INFRASTRUCTURE)

    def test_an_infrastructure_failure_does_not_charge_the_ceiling(self) -> None:
        # The agent was never launched, so no provider call was reachable and
        # the episode's cost is a known zero rather than an unknown that would
        # abandon the sweep.
        episodes = [
            ScriptedEpisode(instance_id=TINY_IDS[0], mode="curated", container_failure="boom")
        ]
        result, _ = self.sweep(instance_ids=[TINY_IDS[0]], modes=["curated"], episodes=episodes)
        self.assertIsNone(result.manifest["budget"]["halt_reason"])
        self.assertEqual(result.manifest["budget"]["unknown_cost_episodes"], 0)

    def test_a_failed_turn_is_a_result_and_is_not_retried(self) -> None:
        episodes = [
            ScriptedEpisode(
                instance_id=TINY_IDS[0], mode="curated", status="failed", exit_code=1,
                estimated_cost_usd=0.01,
            )
        ]
        result, _ = self.sweep(instance_ids=[TINY_IDS[0]], modes=["curated"], episodes=episodes)
        records = read_jsonl(result.run_dir / "episodes.jsonl")
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["termination"], "agent-failed")


class TheGraderIsInvokedOncePerCellWithADistinctRunId(Sweeping):
    """Cache safety, observed at the level of the commands actually issued."""

    def test_each_cell_gets_its_own_run_id_and_its_own_predictions_file(self) -> None:
        result, runner = self.sweep()
        lines = issued_lines(runner)
        grading = [line for line in lines if "run_evaluation" in line]
        self.assertEqual(len(grading), len(MODES))
        for mode in MODES:
            expected = grader_run_id("sbml-demo", mode, 1)
            self.assertTrue(any(f"--run_id {expected}" in line for line in grading), expected)
            self.assertTrue((result.run_dir / "predictions" / mode / "r1.jsonl").is_file())

    def test_the_merged_predictions_file_holds_every_cell(self) -> None:
        result, _ = self.sweep()
        merged = read_jsonl(result.run_dir / "predictions.jsonl")
        self.assertEqual(len(merged), len(TINY_IDS) * len(MODES))
        self.assertEqual(
            len({record["model_name_or_path"] for record in merged}), len(MODES)
        )

    def test_a_cell_grades_only_what_it_attempted(self) -> None:
        result, runner = self.sweep(instance_ids=[TINY_IDS[1]], modes=["both"])
        grading = [line for line in issued_lines(runner) if "run_evaluation" in line]
        self.assertEqual(len(grading), 1)
        self.assertIn(TINY_IDS[1], grading[0])
        self.assertNotIn(TINY_IDS[0], grading[0])


class TheSummaryReproducesFromTheEvidenceAlone(Sweeping):
    """``recompute`` reads the manifest, the episodes and the grader output."""

    def test_recompute_byte_reproduces_the_summary_it_finds(self) -> None:
        result, _ = self.sweep(resolved=[(TINY_IDS[0], "curated")])
        summary, rendered, matches, drifted = pack_recompute(result.run_dir)
        self.assertTrue(matches)
        self.assertEqual(drifted, [])
        self.assertEqual(rendered, (result.run_dir / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["conclusion"], CONCLUSION_PLUMBING)

    def test_recompute_does_not_read_the_summary_it_is_checking(self) -> None:
        # Deleting the summary must not change what recompute derives; if it
        # read it, this would fail or silently agree with whatever was there.
        result, _ = self.sweep(resolved=[(TINY_IDS[0], "both")])
        before = (result.run_dir / "summary.json").read_text(encoding="utf-8")
        (result.run_dir / "summary.json").unlink()
        _, rendered, matches, _ = pack_recompute(result.run_dir)
        self.assertFalse(matches)
        self.assertEqual(rendered, before)

    def test_a_tampered_episode_file_is_reported_as_drift(self) -> None:
        # The digests in the manifest are what make the directory evidence
        # rather than a folder of editable JSON.
        result, _ = self.sweep()
        path = result.run_dir / "episodes.jsonl"
        records = read_jsonl(path)
        self.assertEqual(records[0]["termination"], TERMINATION_COMPLETED)
        records[0]["termination"] = "tampered"
        path.write_text(
            "".join(json.dumps(r, sort_keys=True, ensure_ascii=False) + "\n" for r in records),
            encoding="utf-8",
        )
        _, _, _, drifted = pack_recompute(result.run_dir)
        self.assertIn("episodes.jsonl", drifted)

    def test_an_untouched_directory_reports_no_drift(self) -> None:
        result, _ = self.sweep()
        self.assertEqual(pack_recompute(result.run_dir)[3], [])

    def test_the_manifest_records_the_digests_of_what_it_produced(self) -> None:
        result, _ = self.sweep()
        outputs = result.manifest["outputs"]
        for relative in ("instance_ids.txt", "episodes.jsonl", "predictions.jsonl", "grader/results.json"):
            self.assertIsNotNone(outputs[relative], relative)
            self.assertEqual(len(outputs[relative]), 64)


class NothingUndeclaredReachesTheRecords(Sweeping):
    """The storage contract, checked over a whole sweep's output."""

    def test_the_manifest_records_variable_names_and_never_values(self) -> None:
        result, _ = self.sweep()
        self.assertEqual(
            result.manifest["config"]["allowed_env_names"],
            [
                "ANTHROPIC_API_KEY",
                "POLARIS_CODE_MODEL",
                "POLARIS_CODE_GRAPH_SEARCH",
                "POLARIS_CODE_TOPOLOGY_PIN",
                "POLARIS_CODE_COMPOSITION_POLICY",
                "POLARIS_CODE_GATE_PLANNER",
                "POLARIS_CODE_MAX_TURNS",
                "POLARIS_CODE_MAX_OUTPUT_TOKENS",
                "POLARIS_CODE_DOMAIN",
                "POLARIS_CODE_ARM",
            ],
        )
        body = (result.run_dir / "manifest.json").read_text(encoding="utf-8")
        self.assertNotIn("sk-ant", body)

    def test_no_episode_record_carries_the_issue_text(self) -> None:
        result, _ = self.sweep()
        body = (result.run_dir / "episodes.jsonl").read_text(encoding="utf-8")
        for instance in self.suite.instances:
            self.assertNotIn(instance.problem_statement.strip(), body)
        self.assertIn("<problem-statement>", body)

    def test_the_manifest_names_the_process_runner_that_produced_the_run(self) -> None:
        result, _ = self.sweep()
        self.assertEqual(result.manifest["config"]["process_runner"], "replay")
        self.assertEqual(result.summary["conclusion"], CONCLUSION_PLUMBING)

    def test_the_command_line_is_recorded_for_reproduction(self) -> None:
        result, _ = self.sweep()
        self.assertIn("--output", result.manifest["command_line"])


if __name__ == "__main__":
    unittest.main()
