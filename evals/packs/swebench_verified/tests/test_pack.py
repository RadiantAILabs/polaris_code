"""The Verified pack exposes only dataset facts and official grading."""

from __future__ import annotations

import argparse
import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from ....core.config import AXIS_COMPOSITION, GATE_PLANNER_VAR, RunConfig
from ....core.process import ReplayRunner, ReplayStep
from ....core.report import CellReport
from ..environment import EpisodeRunner
from ..grader import GRADER_MODULE, Grader, grader_argv, grader_run_id, model_name
from ..pack import DEFAULT_API_COST_CEILING_USD, SwebenchVerifiedPack, _gold, notes_digest
from ..suite import Suite

def _run_config(**overrides: object) -> RunConfig:
    """A minimal, replay-mode `RunConfig` a domain/arm wiring test can build on."""
    fields = dict(
        eval_run_id="verified-domain",
        dataset_pack="swebench-verified",
        model="anthropic/claude-sonnet-4-6",
        modes=("both",),
        repetitions=1,
        order_seed=1,
        analysis_seed=1,
        instance_ids=(),
        episode_timeout_seconds=1800,
        grade_timeout_seconds=1800,
        max_workers=1,
        api_cost_ceiling_usd=1.0,
        polaris_code_binary="/nonexistent/polaris-code",
        container_runtime="docker",
        allow_edit=True,
        allow_command=(),
        topology_work=False,
        process_runner="replay",
    )
    fields.update(overrides)
    return RunConfig(**fields)  # type: ignore[arg-type]


class DomainNotesDigest(unittest.TestCase):
    """`notes_digest` reads the checked-in file, never the agent's own report."""

    def test_generic_reads_no_notes_regardless_of_domain(self) -> None:
        for domain in ("none", "django", "sympy", "sphinx"):
            self.assertIsNone(notes_digest(domain, "generic"))

    def test_none_domain_reads_no_notes_regardless_of_arm(self) -> None:
        for arm in ("generic-notes", "expert", "experience"):
            self.assertIsNone(notes_digest("none", arm))

    def test_generic_notes_and_expert_read_the_same_file(self) -> None:
        # The generic-notes and expert arms must carry byte-identical notes
        # text (see `docs/gate-planning.md`'s "Domain plans and arms"
        # section); reading the same digest for both is what a divergence
        # would break first.
        for domain in ("django", "sympy", "sphinx"):
            with self.subTest(domain=domain):
                self.assertEqual(
                    notes_digest(domain, "generic-notes"), notes_digest(domain, "expert")
                )

    def test_composed_records_the_expert_notes_digest(self) -> None:
        for domain in ("django", "sympy", "sphinx"):
            self.assertEqual(notes_digest(domain, "composed"), notes_digest(domain, "expert"))


class DomainAndArmReachTheEpisodeRunner(unittest.TestCase):
    """`SwebenchVerifiedPack.environment()` wires `RunConfig.domain`/`arm` through."""

    def setUp(self) -> None:
        self.pack = SwebenchVerifiedPack()
        self.suite = Suite.load()

    def test_domain_arm_and_notes_digest_reach_the_runner(self) -> None:
        config = _run_config(domain="sympy", arm="expert")
        env = self.pack.environment(
            config=config, suite=self.suite, process_runner=ReplayRunner([]), api_key=None,
        )
        self.assertIsInstance(env, EpisodeRunner)
        self.assertEqual(env._domain, "sympy")  # noqa: SLF001
        self.assertEqual(env._arm, "expert")  # noqa: SLF001
        self.assertEqual(env._notes_digest, notes_digest("sympy", "expert"))  # noqa: SLF001

    def test_the_unmodified_default_reaches_the_runner_too(self) -> None:
        config = _run_config()
        env = self.pack.environment(
            config=config, suite=self.suite, process_runner=ReplayRunner([]), api_key=None,
        )
        self.assertEqual(env._domain, "none")  # noqa: SLF001
        self.assertEqual(env._arm, "generic")  # noqa: SLF001
        self.assertIsNone(env._notes_digest)  # noqa: SLF001


class LeanEnvPassthroughReachesTheEpisodeRunner(unittest.TestCase):
    """A run's recorded `lean_env_names` are read back from the live host
    environment and reach the runner as name/value pairs, never invented and
    never silently dropped when the host no longer has one of them set."""

    def setUp(self) -> None:
        self.pack = SwebenchVerifiedPack()
        self.suite = Suite.load()

    def test_names_present_on_the_host_reach_the_runner_with_their_values(self) -> None:
        config = _run_config(
            lean_env_names=("POLARIS_CODE_LEAN_TWM", "POLARIS_CODE_MAX_COST_USD"),
        )
        with patch.dict(
            "os.environ",
            {"POLARIS_CODE_LEAN_TWM": "1", "POLARIS_CODE_MAX_COST_USD": "0.5"},
        ):
            env = self.pack.environment(
                config=config, suite=self.suite, process_runner=ReplayRunner([]), api_key=None,
            )
        self.assertEqual(
            env._lean_env,  # noqa: SLF001
            {"POLARIS_CODE_LEAN_TWM": "1", "POLARIS_CODE_MAX_COST_USD": "0.5"},
        )

    def test_a_recorded_name_no_longer_set_on_the_host_is_omitted_not_empty(self) -> None:
        config = _run_config(lean_env_names=("POLARIS_CODE_LEAN_TWM",))
        with patch.dict("os.environ", {}, clear=False):
            import os as _os

            _os.environ.pop("POLARIS_CODE_LEAN_TWM", None)
            env = self.pack.environment(
                config=config, suite=self.suite, process_runner=ReplayRunner([]), api_key=None,
            )
        self.assertEqual(env._lean_env, {})  # noqa: SLF001

    def test_no_recorded_names_reaches_the_runner_as_an_empty_mapping(self) -> None:
        config = _run_config()
        env = self.pack.environment(
            config=config, suite=self.suite, process_runner=ReplayRunner([]), api_key=None,
        )
        self.assertEqual(env._lean_env, {})  # noqa: SLF001


class VerifiedPackPins(unittest.TestCase):
    """Run arguments state real pins and do not invent a global task repo."""

    def setUp(self) -> None:
        self.pack = SwebenchVerifiedPack()
        self.suite = Suite.load()

    def test_no_agent_spend_is_enabled_by_default(self) -> None:
        self.assertEqual(self.pack.default_api_cost_ceiling_usd(), DEFAULT_API_COST_CEILING_USD)
        self.assertEqual(DEFAULT_API_COST_CEILING_USD, 0.0)

    def test_matching_dataset_grader_and_language_are_accepted(self) -> None:
        options = argparse.Namespace(
            dataset=self.suite.dataset,
            dataset_revision=self.suite.dataset_revision,
            grader_revision=self.suite.grader_revision,
            language=self.suite.language,
        )
        self.assertIsNone(self.pack.check_run_arguments(options, self.suite))

    def test_the_only_global_pins_are_dataset_and_grader(self) -> None:
        parser = argparse.ArgumentParser()
        self.pack.add_run_arguments(parser)
        options = parser.parse_args([])
        self.assertFalse(hasattr(options, "task_repo_revision"))
        self.assertTrue(self.pack.policy()["human_validated"])

    def test_composition_manifest_records_the_intent_and_environment_names_not_values(self) -> None:
        manifest_config = RunConfig(
            eval_run_id="verified-composition",
            dataset_pack=self.pack.name,
            model="anthropic/claude-sonnet-4-6",
            modes=("gates", "gates-model"),
            repetitions=1,
            order_seed=1,
            analysis_seed=1,
            instance_ids=(),
            episode_timeout_seconds=1800,
            grade_timeout_seconds=1800,
            max_workers=1,
            api_cost_ceiling_usd=1.0,
            polaris_code_binary="/nonexistent/polaris-code",
            container_runtime="docker",
            allow_edit=True,
            allow_command=(),
            topology_work=True,
            process_runner="replay",
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
            topology_intent="careful",
        ).to_json()
        self.assertEqual(manifest_config["topology_intent"], "careful")
        self.assertIn(GATE_PLANNER_VAR, manifest_config["allowed_env_names"])
        self.assertNotIn("scripted", manifest_config["allowed_env_names"])
        self.assertNotIn("model", manifest_config["allowed_env_names"])


class GoldCommandUsesTheOfficialGrader(unittest.TestCase):
    """A passing gold result comes from the adapter, never a local rule."""

    def test_the_official_cli_is_constructed_for_only_the_gold_instance(self) -> None:
        argv = grader_argv(
            dataset="SWE-bench/SWE-bench_Verified",
            predictions_path=Path("out/predictions.jsonl"),
            instance_ids=["sympy__sympy-22914"],
            max_workers=1,
            timeout=1800,
            run_id="verified-gold-gold-r1",
            python="grader-python",
        )
        self.assertEqual(argv[:3], ("grader-python", "-m", GRADER_MODULE))
        self.assertIn("sympy__sympy-22914", argv)
        self.assertIn("SWE-bench/SWE-bench_Verified", argv)

    def test_a_resolved_official_report_is_the_only_passing_gold_verdict(self) -> None:
        report = CellReport(
            mode="gold",
            repetition=1,
            run_id="verified-gold-gold-r1",
            resolved_ids=frozenset({"sympy__sympy-22914"}),
            unresolved_ids=frozenset(),
            empty_patch_ids=frozenset(),
            error_ids=frozenset(),
            graded=True,
            exit_code=0,
        )
        with tempfile.TemporaryDirectory() as raw, patch(
            "evals.packs.swebench_verified.pack.Grader"
        ) as grader_type:
            grader_type.return_value.grade.return_value = report
            options = SimpleNamespace(
                output=str(Path(raw) / "gold"),
                max_workers=1,
                grade_timeout_seconds=1800,
                grader_python="python",
            )
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(_gold(options, Suite.load()), 0)
            grader_type.return_value.grade.assert_called_once()
            grader_type.return_value.write_merged_predictions.assert_called_once()

    def test_an_unresolved_report_is_not_promoted_to_a_pass(self) -> None:
        report = CellReport(
            mode="gold",
            repetition=1,
            run_id="verified-gold-gold-r1",
            resolved_ids=frozenset(),
            unresolved_ids=frozenset({"sympy__sympy-22914"}),
            empty_patch_ids=frozenset(),
            error_ids=frozenset(),
            graded=True,
            exit_code=0,
        )
        with tempfile.TemporaryDirectory() as raw, patch(
            "evals.packs.swebench_verified.pack.Grader"
        ) as grader_type:
            grader_type.return_value.grade.return_value = report
            options = SimpleNamespace(
                output=str(Path(raw) / "gold"),
                max_workers=1,
                grade_timeout_seconds=1800,
                grader_python="python",
            )
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(_gold(options, Suite.load()), 1)

    def test_the_pinned_evaluators_nested_results_file_is_read(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            run_id = grader_run_id("verified-gold", "gold", 1)
            model = model_name("verified-gold", "gold", 1)
            runner = ReplayRunner(
                [
                    ReplayStep(
                        match=("run_evaluation",),
                        writes=(
                            (
                                f"logs/evaluation/{run_id}/results.json",
                                '{"resolved_ids": ["sympy__sympy-22914"]}',
                            ),
                        ),
                    )
                ]
            )
            report = Grader(
                runner,
                dataset="SWE-bench/SWE-bench_Verified",
                max_workers=1,
                timeout=30,
            ).grade(
                eval_run_id="verified-gold",
                mode="gold",
                repetition=1,
                patches={"sympy__sympy-22914": "diff --git a/x b/x\n"},
                instance_ids=["sympy__sympy-22914"],
                run_dir=root,
            )
            self.assertTrue(report.graded)
            self.assertEqual(report.resolved_ids, frozenset({"sympy__sympy-22914"}))


if __name__ == "__main__":
    unittest.main()
