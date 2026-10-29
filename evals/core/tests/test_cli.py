"""Adding a dataset is adding a pack, proved by adding one here.

The pack in this file is not a dataset. It has two made-up instances, an
environment that hands back a canned patch and a grader that resolves whatever
it is given. Nothing about SWE-bench appears in it, and no file under
``evals/core`` knows it exists — it reaches the harness only through
:func:`evals.core.cli.main`.

If a future dataset needed a change inside ``evals/core`` to run, this test
would be the one that could not have been written.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import os
import tempfile
import unittest
import unittest.mock
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Mapping, Sequence

from ..cli import build_parser, main
from ..config import PORTABLE_BINARY_PATH, lean_passthrough_names
from ..episode import EpisodeOutcome, TERMINATION_COMPLETED, outcome
from ..pack import DatasetPack
from ..report import CellReport
from ..usage import RunUsage


@dataclass(frozen=True)
class ToyInstance:
    """One made-up unit of work."""

    instance_id: str
    problem_statement: str


class ToySuite:
    """Two instances and a fixed denominator of two."""

    instances = (
        ToyInstance("one", "make it work\n"),
        ToyInstance("two", "make it fast\n"),
    )

    @property
    def denominator(self) -> int:
        return len(self.instances)

    @property
    def instance_ids(self) -> tuple[str, ...]:
        return tuple(i.instance_id for i in self.instances)

    def select(self, instance_ids: Sequence[str]) -> tuple[ToyInstance, ...]:
        wanted = set(instance_ids)
        if not wanted:
            return self.instances
        return tuple(i for i in self.instances if i.instance_id in wanted)

    def revisions(self) -> dict[str, Any]:
        return {
            "denominator": self.denominator,
            "instance_count": len(self.instances),
            "toy_revision": "v1",
        }


class ToyEnvironment:
    """Hands back a canned patch and a priced usage record."""

    def run(
        self,
        *,
        instance: ToyInstance,
        mode: str,
        repetition: int,
        attempt: int,
        eval_run_id: str,
        model: str,
    ) -> EpisodeOutcome:
        return outcome(
            instance_id=instance.instance_id,
            mode=mode,
            repetition=repetition,
            attempt=attempt,
            session=f"{eval_run_id}-{mode}-{repetition}-{instance.instance_id}",
            termination=TERMINATION_COMPLETED,
            exit_code=0,
            turn_number=1,
            run_id="toy-run",
            status="completed",
            usage=RunUsage(
                model_id=model,
                input_tokens=10,
                output_tokens=2,
                cache_read_tokens=0,
                cache_creation_tokens=0,
                total_tokens=12,
                estimated_cost_usd=0.01,
                pricing_basis="toy",
                pricing={"family": "toy"},
            ),
            max_output_tokens=4096,
            patch=f"--- a\n+++ b\n@@\n+{mode}\n",
            image="toy:latest",
            image_digest=None,
            base_commit="0" * 40,
            head_commit="0" * 40,
            tree_was_clean=True,
            error=None,
            seconds=0.1,
            commands=("toy run",),
        )


class ToyGrader:
    """Resolves the first instance of every cell and nothing else."""

    def grade(
        self,
        *,
        eval_run_id: str,
        mode: str,
        repetition: int,
        patches: Mapping[str, str],
        instance_ids: Sequence[str],
        run_dir: Path,
    ) -> CellReport:
        resolved = frozenset(instance_ids[:1])
        return CellReport(
            mode=mode,
            repetition=repetition,
            run_id=f"{eval_run_id}-{mode}-r{repetition}",
            resolved_ids=resolved,
            unresolved_ids=frozenset(instance_ids) - resolved,
            empty_patch_ids=frozenset(),
            error_ids=frozenset(),
            graded=True,
            exit_code=0,
        )

    def write_merged_predictions(self, run_dir: Path) -> None:
        (run_dir / "toy-predictions.json").write_text("{}\n", encoding="utf-8")


class ToyPack(DatasetPack):
    """A dataset pack that is not a dataset."""

    name = "toy"
    version = "0.1.0"

    def load_suite(self) -> ToySuite:
        return ToySuite()

    def policy(self) -> dict[str, Any]:
        return {"split": "none", "problem_statement_licence": "public-domain"}

    def default_api_cost_ceiling_usd(self) -> float:
        return 5.0

    def summary_suite_keys(self) -> tuple[str, ...]:
        return ("denominator", "toy_revision")

    def digested_outputs(self) -> tuple[str, ...]:
        return ("toy-predictions.json",)

    def environment(self, **_kwargs: Any) -> ToyEnvironment:
        return ToyEnvironment()

    def grader(self, **_kwargs: Any) -> ToyGrader:
        return ToyGrader()

    def add_run_arguments(self, parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--toy-flag", default=None)


def invoke(argv: Sequence[str]) -> tuple[int, str, str]:
    """Run the shared CLI with the toy pack, capturing both streams."""
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = main(ToyPack(), list(argv))
    return code, out.getvalue(), err.getvalue()


class ASecondPackNeedsNoCoreChange(unittest.TestCase):
    """A whole sweep, a whole summary and a whole recompute, from one class."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)

    def test_portable_binary_is_the_run_and_preflight_default(self) -> None:
        """A normal harness invocation never falls back to host-glibc output."""
        parser = build_parser(ToyPack())
        run = parser.parse_args(["run", "--output", str(self.root / "run")])
        preflight = parser.parse_args(["preflight"])
        self.assertEqual(run.polaris_code, PORTABLE_BINARY_PATH)
        self.assertEqual(preflight.polaris_code, PORTABLE_BINARY_PATH)

    def test_turn_and_output_limits_default_to_the_binarys_and_refuse_non_positive(self) -> None:
        parser = build_parser(ToyPack())
        run = parser.parse_args(["run", "--output", str(self.root / "run")])
        self.assertEqual((run.max_turns, run.max_output_tokens), (100, 16384))
        run = parser.parse_args([
            "run", "--output", str(self.root / "run"), "--max-turns", "40", "--max-output-tokens", "8000",
        ])
        self.assertEqual((run.max_turns, run.max_output_tokens), (40, 8000))
        for flag in ("--max-turns", "--max-output-tokens"):
            for bad in ("0", "-3", "lots"):
                with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                    parser.parse_args(["run", "--output", str(self.root / "run"), flag, bad])

    def test_transcripts_are_kept_only_when_asked(self) -> None:
        parser = build_parser(ToyPack())
        run = parser.parse_args(["run", "--output", str(self.root / "run")])
        self.assertFalse(run.keep_transcripts)
        run = parser.parse_args(["run", "--output", str(self.root / "run"), "--keep-transcripts"])
        self.assertTrue(run.keep_transcripts)

    def sweep(self, name: str = "toy-run") -> tuple[int, Path]:
        run_dir = self.root / name
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary", "--allow-dirty",
            "--modes", "curated,freeform", "--quiet",
        ])
        return code, run_dir

    def test_a_sweep_runs_end_to_end_and_scores_the_packs_denominator(self) -> None:
        code, run_dir = self.sweep()
        self.assertEqual(code, 0)
        summary = json.loads((run_dir / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["suite"], {"denominator": 2, "toy_revision": "v1"})
        for cell in summary["cells"]:
            self.assertEqual(cell["resolved"], 1)
            self.assertAlmostEqual(cell["resolved_rate"], 50.0)

    def test_the_manifest_names_the_pack_that_produced_the_run(self) -> None:
        _, run_dir = self.sweep()
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["dataset_pack"], "toy")
        self.assertEqual(manifest["pack"]["name"], "toy")
        self.assertEqual(manifest["pack"]["version"], "0.1.0")
        self.assertEqual(manifest["pack"]["policy"]["split"], "none")

    def test_the_packs_own_artifact_is_digested(self) -> None:
        _, run_dir = self.sweep()
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(len(manifest["outputs"]["toy-predictions.json"]), 64)

    def test_recompute_reproduces_this_packs_summary_too(self) -> None:
        _, run_dir = self.sweep()
        code, stdout, _ = invoke(["recompute", str(run_dir)])
        self.assertEqual(code, 0)
        self.assertIn("reproduces byte-for-byte", stdout)

    def test_recompute_refuses_a_run_another_pack_produced(self) -> None:
        # The manifest names its pack, so recomputing it with the wrong one is
        # refused rather than answered with a differently-shaped summary.
        _, run_dir = self.sweep()
        manifest_path = run_dir / "manifest.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["config"]["dataset_pack"] = "some-other-pack"
        manifest_path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        code, _, stderr = invoke(["recompute", str(run_dir)])
        self.assertEqual(code, 2)
        self.assertIn("some-other-pack", stderr)

    def test_the_ceiling_default_is_the_packs_own(self) -> None:
        code, _, stderr = invoke([
            "run", "--output", str(self.root / "expensive"),
            "--polaris-code", "/nonexistent/binary", "--allow-dirty",
            "--repetitions", "1000", "--quiet",
        ])
        self.assertEqual(code, 2)
        self.assertIn("5.00", stderr)

    def test_the_packs_own_flag_reaches_the_parser(self) -> None:
        code, _, _ = invoke([
            "run", "--output", str(self.root / "flagged"),
            "--polaris-code", "/nonexistent/binary", "--allow-dirty",
            "--modes", "curated", "--toy-flag", "x", "--quiet",
        ])
        self.assertEqual(code, 0)


class DomainAndArmSelectionIsValidatedAndRecorded(unittest.TestCase):
    """The domain/arm pair is a run-wide config, not a sweep axis."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)

    def test_unset_domain_and_arm_default_to_the_unmodified_generic_plan(self) -> None:
        run_dir = self.root / "default"
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--modes", "curated", "--quiet",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["domain"], "none")
        self.assertEqual(manifest["config"]["arm"], "generic")

    def test_an_explicit_domain_and_arm_are_recorded_in_the_manifest(self) -> None:
        run_dir = self.root / "django-expert"
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--modes", "curated", "--quiet",
            "--domain", "django", "--arm", "expert",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["domain"], "django")
        self.assertEqual(manifest["config"]["arm"], "expert")

    def test_an_unknown_domain_or_arm_is_refused_before_a_run_directory_exists(self) -> None:
        # `choices=` on the flag itself refuses an unrecognised value before
        # `main` is even reached, the same way an out-of-range `--max-turns`
        # already does (`argparse` raises `SystemExit(2)` directly).
        parser = build_parser(ToyPack())
        for flag, value in (("--domain", "djangoo"), ("--arm", "expertt")):
            with self.subTest(flag=flag):
                output = self.root / f"bad-{flag.strip('-')}"
                with self.assertRaises(SystemExit) as raised, contextlib.redirect_stderr(io.StringIO()):
                    parser.parse_args(["run", "--output", str(output), flag, value])
                self.assertEqual(raised.exception.code, 2)
                self.assertFalse(output.exists())

    def test_an_arm_that_needs_a_domain_is_refused_without_one(self) -> None:
        for arm in ("generic-notes", "expert", "experience"):
            with self.subTest(arm=arm):
                output = self.root / f"needs-domain-{arm}"
                code, _, stderr = invoke([
                    "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                    "--allow-dirty", "--arm", arm,
                ])
                self.assertEqual(code, 2)
                self.assertIn(arm, stderr)
                self.assertIn("needs a domain", stderr)
                self.assertFalse(output.exists())

    def test_the_loop_arm_is_accepted_with_domain_none_and_recorded(self) -> None:
        run_dir = self.root / "loop"
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--modes", "curated", "--quiet", "--arm", "loop",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["domain"], "none")
        self.assertEqual(manifest["config"]["arm"], "loop")

    def test_the_loop_arm_is_refused_alongside_a_real_domain(self) -> None:
        output = self.root / "loop-with-domain"
        code, _, stderr = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--domain", "django", "--arm", "loop",
        ])
        self.assertEqual(code, 2)
        self.assertIn("loop", stderr)
        self.assertFalse(output.exists())

    def test_the_lean_arm_is_accepted_with_domain_none_and_recorded(self) -> None:
        run_dir = self.root / "lean"
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--modes", "curated", "--quiet", "--arm", "lean",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["domain"], "none")
        self.assertEqual(manifest["config"]["arm"], "lean")

    def test_the_lean_arm_is_refused_alongside_a_real_domain(self) -> None:
        output = self.root / "lean-with-domain"
        code, _, stderr = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--domain", "django", "--arm", "lean",
        ])
        self.assertEqual(code, 2)
        self.assertIn("lean", stderr)
        self.assertFalse(output.exists())

    def test_composed_requires_a_domain_and_only_gates_model(self) -> None:
        for extra in (
            ["--experiment-axis", "composition", "--composition-policies", "gates-model"],
            ["--experiment-axis", "composition", "--composition-policies", "gates"],
        ):
            output = self.root / ("composed-" + extra[-1])
            code, _, stderr = invoke([
                "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                "--allow-dirty", "--domain", "none", "--arm", "composed", *extra,
            ])
            self.assertEqual(code, 2)
            self.assertIn("domain", stderr)
            self.assertFalse(output.exists())

        output = self.root / "composed-wrong-policy"
        code, _, stderr = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--domain", "django", "--arm", "composed",
            "--experiment-axis", "composition", "--composition-policies", "gates",
        ])
        self.assertEqual(code, 2)
        self.assertIn("gates-model", stderr)
        self.assertFalse(output.exists())

    def test_composed_is_accepted_and_recorded_with_gates_model(self) -> None:
        output = self.root / "composed"
        code, _, stderr = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--quiet", "--domain", "django", "--arm", "composed",
            "--experiment-axis", "composition", "--composition-policies", "gates-model",
        ])
        self.assertEqual(code, 0, stderr)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["arm"], "composed")
        self.assertEqual(manifest["config"]["modes"], ["gates-model"])

    def test_the_fine_arm_is_accepted_and_recorded_only_without_a_domain(self) -> None:
        run_dir = self.root / "fine"
        code, _, _ = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--modes", "curated", "--quiet", "--arm", "fine",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["arm"], "fine")
        self.assertEqual(manifest["config"]["domain"], "none")
        refused = self.root / "fine-with-domain"
        code, _, stderr = invoke([
            "run", "--output", str(refused), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--domain", "django", "--arm", "fine",
        ])
        self.assertEqual(code, 2)
        self.assertIn("fine", stderr)
        self.assertFalse(refused.exists())

    def test_fine_context_arm_is_recorded_only_without_a_domain(self) -> None:
        run_dir = self.root / "fine-ctx"
        code, _, stderr = invoke([
            "run", "--output", str(run_dir), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--experiment-axis", "composition",
            "--composition-policies", "gates", "--quiet",
            "--domain", "none", "--arm", "fine-ctx",
        ])
        self.assertEqual(code, 0, stderr)
        manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["arm"], "fine-ctx")
        self.assertEqual(manifest["config"]["domain"], "none")
        self.assertEqual(manifest["config"]["modes"], ["gates"])
        refused = self.root / "fine-ctx-with-domain"
        code, _, stderr = invoke([
            "run", "--output", str(refused), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--domain", "django", "--arm", "fine-ctx",
        ])
        self.assertEqual(code, 2)
        self.assertIn("fine-ctx", stderr)
        self.assertFalse(refused.exists())


class ExperimentAxisArgumentsAreValidated(unittest.TestCase):
    """The shared CLI validates an axis before the runner can start work."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)

    def test_unknown_and_repeated_topologies_are_refused_before_a_run_directory_exists(self) -> None:
        for value, expected in (
            ("code.unknown", "unknown topology"),
            ("code.direct-edit,code.direct-edit", "selected more than once"),
        ):
            with self.subTest(value=value):
                output = self.root / value.replace(",", "-")
                code, _, stderr = invoke([
                    "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                    "--allow-dirty", "--experiment-axis", "topology", "--topologies", value,
                ])
                self.assertEqual(code, 2)
                self.assertIn(expected, stderr)
                self.assertFalse(output.exists())

    def test_a_topology_sweep_records_its_fixed_control_and_all_pairs(self) -> None:
        output = self.root / "topology"
        code, _, _ = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--quiet", "--experiment-axis", "topology",
            "--topologies", "code.tree-question,code.direct-edit,code.coverage-audit",
            "--graph-search-mode", "both",
            "--api-cost-ceiling-usd", "10",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["axis"], "topology")
        self.assertEqual(manifest["config"]["graph_search_mode"], "both")
        self.assertEqual(
            manifest["config"]["modes"],
            ["code.direct-edit", "code.coverage-audit", "code.tree-question"],
        )
        summary = json.loads((output / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(
            [comparison["pair"] for comparison in summary["comparisons"]],
            [
                "code.direct-edit-code.coverage-audit",
                "code.direct-edit-code.tree-question",
                "code.coverage-audit-code.tree-question",
            ],
        )
        self.assertTrue(all(not comparison["credible"] for comparison in summary["comparisons"]))
        self.assertEqual(summary["conclusion"], "not distinguished at this budget")

    def test_crossing_the_axes_is_refused_instead_of_quietly_multiplying_cost(self) -> None:
        code, _, stderr = invoke([
            "run", "--output", str(self.root / "crossed"), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--experiment-axis", "topology",
            "--topologies", "code.direct-edit,code.tree-question", "--modes", "curated,both",
        ])
        self.assertEqual(code, 2)
        self.assertIn("cannot cross graph-search and topology axes", stderr)

    def test_unknown_and_repeated_composition_policies_are_refused_before_a_run_directory_exists(
        self,
    ) -> None:
        for value, expected in (
            ("bogus", "unknown composition policy"),
            ("model,model", "selected more than once"),
        ):
            with self.subTest(value=value):
                output = self.root / value.replace(",", "-")
                code, _, stderr = invoke([
                    "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                    "--allow-dirty", "--experiment-axis", "composition",
                    "--composition-policies", value,
                ])
                self.assertEqual(code, 2)
                self.assertIn(expected, stderr)
                self.assertFalse(output.exists())

    def test_a_composition_sweep_records_its_fixed_control_and_all_pairs(self) -> None:
        output = self.root / "composition"
        code, _, _ = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--quiet", "--experiment-axis", "composition",
            "--composition-policies", "deterministic,model",
            "--graph-search-mode", "both",
            "--api-cost-ceiling-usd", "10",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["axis"], "composition")
        self.assertEqual(manifest["config"]["graph_search_mode"], "both")
        self.assertEqual(manifest["config"]["modes"], ["deterministic", "model"])
        summary = json.loads((output / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(
            [comparison["pair"] for comparison in summary["comparisons"]],
            ["deterministic-model"],
        )
        self.assertFalse(summary["comparisons"][0]["credible"])
        self.assertEqual(summary["conclusion"], "not distinguished at this budget")

    def test_gate_composition_cells_and_a_careful_intent_are_recorded(self) -> None:
        output = self.root / "gate-composition"
        code, _, _ = invoke([
            "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--quiet", "--experiment-axis", "composition",
            "--composition-policies", "gates,gates-model", "--graph-search-mode", "both",
            "--topology-intent", "careful", "--api-cost-ceiling-usd", "10",
        ])
        self.assertEqual(code, 0)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["modes"], ["gates", "gates-model"])
        self.assertEqual(manifest["config"]["topology_intent"], "careful")

    def test_crossing_the_graph_search_and_composition_axes_is_refused(self) -> None:
        code, _, stderr = invoke([
            "run", "--output", str(self.root / "crossed-composition"), "--polaris-code", "/nonexistent/binary",
            "--allow-dirty", "--experiment-axis", "composition",
            "--composition-policies", "deterministic,model", "--modes", "curated,both",
        ])
        self.assertEqual(code, 2)
        self.assertIn("cannot cross graph-search and composition axes", stderr)

    def test_composition_policies_requires_the_composition_axis(self) -> None:
        code, _, stderr = invoke([
            "run", "--output", str(self.root / "misplaced-composition-policies"),
            "--polaris-code", "/nonexistent/binary", "--allow-dirty",
            "--composition-policies", "deterministic,model",
        ])
        self.assertEqual(code, 2)
        self.assertIn("--composition-policies requires --experiment-axis composition", stderr)

    def test_topologies_and_composition_policies_cannot_both_be_asked_for(self) -> None:
        code, _, stderr = invoke([
            "run", "--output", str(self.root / "both-axis-flags"),
            "--polaris-code", "/nonexistent/binary", "--allow-dirty",
            "--experiment-axis", "topology", "--topologies", "code.direct-edit",
            "--composition-policies", "deterministic,model",
        ])
        self.assertEqual(code, 2)
        self.assertIn("--composition-policies requires --experiment-axis composition", stderr)

    def test_an_axis_sweep_still_defaults_to_the_portable_binary(self) -> None:
        """Axis sweeps still default to the portable binary when
        ``--polaris-code`` is omitted.
        """
        parser = build_parser(ToyPack())
        topology = parser.parse_args([
            "run", "--output", str(self.root / "topology-default-binary"),
            "--experiment-axis", "topology", "--topologies", "code.direct-edit",
        ])
        composition = parser.parse_args([
            "run", "--output", str(self.root / "composition-default-binary"),
            "--experiment-axis", "composition", "--composition-policies", "deterministic,model",
        ])
        self.assertEqual(topology.polaris_code, PORTABLE_BINARY_PATH)
        self.assertEqual(composition.polaris_code, PORTABLE_BINARY_PATH)


class LeanEnvPassthroughIsRecordedByNameOnly(unittest.TestCase):
    """A lean switch set on the host reaches the manifest as a name, not a value."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)

    def test_lean_and_max_cost_names_present_on_the_host_are_recorded(self) -> None:
        output = self.root / "lean-passthrough"
        with unittest.mock.patch.dict(
            os.environ,
            {
                **_without_passthrough_names(os.environ),
                "POLARIS_CODE_LEAN_TWM": "1",
                "POLARIS_CODE_LEAN_TWM_REBUILD_TURNS": "4",
                "POLARIS_CODE_MAX_COST_USD": "0.75",
                # Deliberately not a match: must not be swept in.
                "POLARIS_CODE_UNRELATED": "ignored",
            },
            clear=True,
        ):
            code, _, _ = invoke([
                "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                "--allow-dirty", "--modes", "curated", "--quiet",
            ])
        self.assertEqual(code, 0)
        manifest_text = (output / "manifest.json").read_text(encoding="utf-8")
        manifest = json.loads(manifest_text)
        self.assertEqual(
            manifest["config"]["lean_env_names"],
            [
                "POLARIS_CODE_LEAN_TWM",
                "POLARIS_CODE_LEAN_TWM_REBUILD_TURNS",
                "POLARIS_CODE_MAX_COST_USD",
            ],
        )
        self.assertNotIn("POLARIS_CODE_UNRELATED", manifest["config"]["lean_env_names"])
        # Only names, never values, anywhere in the manifest.
        self.assertNotIn("0.75", manifest_text)

    def test_no_lean_switches_on_the_host_records_an_empty_list(self) -> None:
        output = self.root / "lean-passthrough-empty"
        scrubbed = _without_passthrough_names(os.environ)
        with unittest.mock.patch.dict(os.environ, scrubbed, clear=True):
            code, _, _ = invoke([
                "run", "--output", str(output), "--polaris-code", "/nonexistent/binary",
                "--allow-dirty", "--modes", "curated", "--quiet",
            ])
        self.assertEqual(code, 0)
        manifest = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["config"]["lean_env_names"], [])


def _without_passthrough_names(environ) -> dict[str, str]:
    """`environ` minus every name the manifest would record, so host settings
    such as ``AWS_REGION`` cannot leak into the assertions."""
    recorded = set(lean_passthrough_names(environ))
    return {key: value for key, value in environ.items() if key not in recorded}


if __name__ == "__main__":
    unittest.main()
