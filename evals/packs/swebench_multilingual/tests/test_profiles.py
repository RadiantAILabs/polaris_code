"""A fast run is a signal you can act on and can never be reported as a result.

The refusal these tests protect is structural, not arithmetic. A run whose
manifest records an iteration profile reports
``iteration-signal-not-evidence`` **whatever its numbers say** — three
repetitions, complete cells and a hundred-point gap included — for the same
reason a replayed run reports ``plumbing-demonstration``. So the tests here
assert the refusal against configurations that would otherwise clear the bar,
and pair each one with the identical configuration *without* the profile, which
does clear it. A refusal that fires on everything proves nothing.

The second thing under test is that the label travels. The recorded
configuration, the summary, every cell and the command's own output all carry
it, because the output of a cheap run is going to be pasted into a message with
its context stripped and that paste has to stay honest on its own.
"""

from __future__ import annotations

import contextlib
import dataclasses
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from ....core import cli
from ....core.budget import BudgetExceeded, CostGuard, HALT_CEILING
from ....core.collate import (
    CONCLUSION_CREDIBLE,
    CONCLUSION_INCOMPLETE,
    CONCLUSION_ITERATION,
    CONCLUSION_PLUMBING,
    summarise,
)
from ....core.config import (
    AXIS_COMPOSITION,
    EVIDENCE_BAR,
    MODES,
    PLANNING_COST_PER_EPISODE_USD,
    not_evidence_label,
)
from ....core.process import REPLAY, SUBPROCESS
from ....core.profiles import IterationProfile, resolve
from ....core.runner import RunDirectoryExists, plan_order
from ....core.tests.test_collate import PINNED, cells, episodes, manifest
from ..pack import SwebenchMultilingualPack
from ..profiles import DEFAULT_PROFILE, PROFILES
from ..suite import SUITE_DENOMINATOR, Suite
from .support import (
    TINY_IDS,
    config,
    pack_recompute,
    read_jsonl,
    replay_sweep,
    scripted_sweep,
    tiny_suite,
)


class ThePinnedProfileIsFixedAndFollowsAStatedRule(unittest.TestCase):
    """An ad-hoc subset makes two people's cheap runs incomparable."""

    def setUp(self) -> None:
        self.suite = Suite.load()
        self.profile = resolve(PROFILES, DEFAULT_PROFILE)

    def test_the_same_named_profile_always_selects_the_same_instances(self) -> None:
        first = resolve(PROFILES, DEFAULT_PROFILE).instance_ids
        second = resolve(PROFILES, DEFAULT_PROFILE).instance_ids
        self.assertEqual(first, second)
        # And the suite hands them back in its own order, not the profile's, so
        # the schedule is a pure function of the recorded configuration.
        selected = [i.instance_id for i in self.suite.select(first)]
        self.assertEqual(tuple(selected), first)

    def test_the_pinned_list_is_the_first_instance_of_every_repository(self) -> None:
        # The stated rule, re-derived from the checked-in suite. The tuple in
        # profiles.py is pinned rather than computed, so this is what keeps the
        # pin and the rule it claims to follow from drifting apart.
        seen: dict[str, str] = {}
        for instance in self.suite.instances:
            seen.setdefault(instance.repo, instance.instance_id)
        self.assertEqual(self.profile.instance_ids, tuple(seen.values()))
        self.assertEqual(len(self.profile.instance_ids), 7)

    def test_every_pinned_id_is_in_the_suite(self) -> None:
        self.assertEqual(
            len(self.suite.select(self.profile.instance_ids)),
            len(self.profile.instance_ids),
        )

    def test_the_profile_is_smaller_than_the_suite_it_samples(self) -> None:
        self.assertLess(len(self.profile.instance_ids), SUITE_DENOMINATOR)
        self.assertEqual(self.profile.repetitions, 1)

    def test_an_unknown_profile_name_names_the_ones_that_exist(self) -> None:
        with self.assertRaises(ValueError) as raised:
            resolve(PROFILES, "no-such-profile")
        self.assertIn(DEFAULT_PROFILE, str(raised.exception))


class CheapnessIsStatedInUnits(unittest.TestCase):
    """"Faster" is not a specification; these are the numbers derived from the
    planning constants.
    """

    def setUp(self) -> None:
        self.profile = resolve(PROFILES, DEFAULT_PROFILE)
        self.full_episodes = SUITE_DENOMINATOR * len(MODES) * 1

    def test_the_full_sweep_baseline_is_43_tasks_times_three_modes(self) -> None:
        self.assertEqual(self.full_episodes, 129)
        self.assertAlmostEqual(
            self.full_episodes * PLANNING_COST_PER_EPISODE_USD, 108.36, places=2
        )

    def test_the_profile_schedules_twenty_one_episodes(self) -> None:
        self.assertEqual(self.profile.episode_count(len(MODES)), 21)

    def test_the_projection_is_a_sixth_of_a_measured_sweep(self) -> None:
        projected = self.profile.episode_count(len(MODES)) * PLANNING_COST_PER_EPISODE_USD
        self.assertAlmostEqual(projected, 17.64, places=2)
        ratio = projected / (self.full_episodes * PLANNING_COST_PER_EPISODE_USD)
        self.assertLess(ratio, 0.20)
        self.assertAlmostEqual(ratio, 0.1628, places=4)

    def test_the_worst_case_wall_clock_is_a_twentieth_of_a_measured_sweep(self) -> None:
        # Episodes run sequentially, so the per-episode timeout times the
        # episode count is the bound a reader can hold the profile to.
        fast = self.profile.episode_count(len(MODES)) * self.profile.episode_timeout_seconds
        full = self.full_episodes * 1800
        self.assertEqual(fast, 12_600)
        self.assertEqual(full, 232_200)
        self.assertLess(fast / full, 0.06)


class AFastRunCanNeverClearTheBar(unittest.TestCase):
    """The refusal is structural: it keys on the recorded profile, not the data."""

    def fast(self, **kwargs) -> dict:
        """A manifest that would clear the bar if the profile were absent."""
        return manifest(iteration_profile=DEFAULT_PROFILE, **kwargs)

    def test_three_complete_sweeps_with_a_hundred_point_gap_are_still_refused(self) -> None:
        summary = summarise(
            manifest=self.fast(repetitions=3, process_runner=SUBPROCESS),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        self.assertTrue(summary["complete"])
        self.assertEqual(summary["conclusion"], CONCLUSION_ITERATION)
        for comparison in summary["comparisons"]:
            self.assertFalse(comparison["eligible"])
            self.assertFalse(comparison["credible"])

    def test_the_same_numbers_without_the_profile_are_credible(self) -> None:
        # The control. Only `iteration_profile` differs, so the refusal is
        # demonstrably the profile and not the data.
        summary = summarise(
            manifest=manifest(repetitions=3, process_runner=SUBPROCESS),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_CREDIBLE)
        self.assertTrue(any(c["credible"] for c in summary["comparisons"]))

    def test_no_repetition_count_or_completeness_lets_a_fast_run_through(self) -> None:
        # Enumerated rather than sampled: whatever a caller does to a fast
        # configuration, `credible difference` must stay unreachable.
        for repetitions in (1, 2, 3, 4):
            for complete in (True, False):
                attempted = PINNED if complete else PINNED[:-1]
                with self.subTest(repetitions=repetitions, complete=complete):
                    summary = summarise(
                        manifest=self.fast(repetitions=repetitions, process_runner=SUBPROCESS),
                        episodes=episodes(repetitions=repetitions, instances=attempted),
                        grader_cells=cells(
                            {"curated": set(attempted)},
                            repetitions=repetitions,
                            instances=attempted,
                        ),
                    )
                    self.assertEqual(summary["conclusion"], CONCLUSION_ITERATION)
                    self.assertNotEqual(summary["conclusion"], CONCLUSION_CREDIBLE)
                    for comparison in summary["comparisons"]:
                        self.assertFalse(comparison["credible"])

    def test_a_replayed_fast_run_is_a_plumbing_demonstration_not_a_signal(self) -> None:
        # Both refusals apply and the stronger one wins: nothing executed, so
        # calling the fixtures a signal would be the larger of the two lies.
        summary = summarise(
            manifest=self.fast(repetitions=3, process_runner=REPLAY),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_PLUMBING)
        self.assertEqual(summary["iteration_profile"], DEFAULT_PROFILE)

    def test_an_ordinary_incomplete_run_keeps_its_own_label(self) -> None:
        # The refusal is targeted: without a profile, a partial run is still
        # `incomplete`, whose remedy is to run it again properly.
        summary = summarise(
            manifest=manifest(process_runner=SUBPROCESS),
            episodes=episodes(instances=PINNED[:3]),
            grader_cells=cells({}, instances=PINNED[:3]),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_INCOMPLETE)
        self.assertIsNone(summary["not_evidence"])


class TheLabelSurvivesBeingCopied(unittest.TestCase):
    """Manifest, summary, every cell and the command's own output."""

    def summary(self) -> dict:
        return summarise(
            manifest=manifest(
                iteration_profile=DEFAULT_PROFILE,
                instance_ids=PINNED[:4],
                repetitions=3,
                process_runner=SUBPROCESS,
            ),
            episodes=episodes(repetitions=3),
            grader_cells=cells({"curated": set(PINNED)}, repetitions=3),
        )

    def test_the_recorded_configuration_carries_the_label_itself(self) -> None:
        # Not only the profile name: a reader holding the manifest alone must
        # not have to look a name up in a table to learn it is not evidence.
        recorded = config(
            output=Path("/tmp/x"), process_runner=SUBPROCESS, iteration_profile=DEFAULT_PROFILE
        ).to_json()
        self.assertEqual(recorded["iteration_profile"], DEFAULT_PROFILE)
        self.assertEqual(recorded["not_evidence"], CONCLUSION_ITERATION)

    def test_a_measured_configuration_says_so_rather_than_saying_nothing(self) -> None:
        recorded = config(output=Path("/tmp/x"), process_runner=SUBPROCESS).to_json()
        self.assertIsNone(recorded["iteration_profile"])
        self.assertIsNone(recorded["not_evidence"])

    def test_a_replayed_configuration_carries_the_replay_label(self) -> None:
        recorded = config(output=Path("/tmp/x")).to_json()
        self.assertEqual(recorded["not_evidence"], CONCLUSION_PLUMBING)

    def test_the_summary_carries_the_label_and_the_profile_name(self) -> None:
        summary = self.summary()
        self.assertEqual(summary["not_evidence"], CONCLUSION_ITERATION)
        self.assertEqual(summary["iteration_profile"], DEFAULT_PROFILE)
        self.assertEqual(summary["conclusion"], CONCLUSION_ITERATION)

    def test_every_cell_carries_the_label_beside_its_figures(self) -> None:
        # A cell is the unit that gets pasted on its own, so `resolved: 10` has
        # to arrive with the reason it is not a score.
        for cell in self.summary()["cells"]:
            self.assertEqual(cell["not_evidence"], CONCLUSION_ITERATION)

    def test_a_measured_cell_says_it_is_not_disqualified(self) -> None:
        summary = summarise(
            manifest=manifest(process_runner=SUBPROCESS),
            episodes=episodes(),
            grader_cells=cells({}),
        )
        for cell in summary["cells"]:
            self.assertIsNone(cell["not_evidence"])

    def test_the_note_states_the_numbers_a_reader_needs(self) -> None:
        note = self.summary()["conclusion_note"]
        self.assertIn(DEFAULT_PROFILE, note)
        self.assertIn("not evidence", note.lower())
        for fragment in ("4 of 10 pinned instances", "3 repetitions"):
            self.assertIn(fragment, note)

    def test_the_note_is_derived_from_the_record_not_from_the_profile_table(self) -> None:
        # `recompute` compares bytes, so a summary whose prose came from the
        # current profiles.py would drift the moment that table was edited.
        summary = summarise(
            manifest=manifest(
                iteration_profile="a-profile-this-build-never-heard-of",
                repetitions=1,
                process_runner=SUBPROCESS,
            ),
            episodes=episodes(),
            grader_cells=cells({}),
        )
        self.assertEqual(summary["conclusion"], CONCLUSION_ITERATION)
        self.assertIn("a-profile-this-build-never-heard-of", summary["conclusion_note"])

    def test_the_commands_own_output_carries_the_label(self) -> None:
        summary = self.summary()
        report = cli.run_report(summary, run_dir=Path("output/example"))
        self.assertEqual(report["not_evidence"], CONCLUSION_ITERATION)
        self.assertEqual(report["iteration_profile"], DEFAULT_PROFILE)
        self.assertEqual(report["conclusion"], CONCLUSION_ITERATION)
        banner = cli.not_evidence_banner(summary)
        self.assertIsNotNone(banner)
        self.assertIn(CONCLUSION_ITERATION, banner)
        self.assertIn(DEFAULT_PROFILE, banner)

    def test_a_measured_run_prints_no_banner_to_shout_down(self) -> None:
        summary = summarise(
            manifest=manifest(process_runner=SUBPROCESS),
            episodes=episodes(),
            grader_cells=cells({}),
        )
        self.assertIsNone(cli.not_evidence_banner(summary))


class TheOneRuleLivesInOnePlace(unittest.TestCase):
    """Four artefacts derive the label; deriving it four times would diverge."""

    def test_replay_wins_over_a_profile(self) -> None:
        self.assertEqual(
            not_evidence_label(process_runner=REPLAY, iteration_profile=DEFAULT_PROFILE),
            CONCLUSION_PLUMBING,
        )

    def test_a_profile_alone_is_an_iteration_signal(self) -> None:
        self.assertEqual(
            not_evidence_label(process_runner=SUBPROCESS, iteration_profile=DEFAULT_PROFILE),
            CONCLUSION_ITERATION,
        )

    def test_a_measured_sweep_has_no_label(self) -> None:
        self.assertIsNone(
            not_evidence_label(process_runner=SUBPROCESS, iteration_profile=None)
        )


class TheBudgetGuardGovernsAFastRunUnchanged(unittest.TestCase):
    """A cheap mode is still a mode that spends, so it gets no carve-out."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.suite = tiny_suite()

    def test_the_profile_carries_no_ceiling_of_its_own(self) -> None:
        self.assertFalse(
            [f.name for f in dataclasses.fields(IterationProfile) if "ceiling" in f.name]
        )

    def test_the_projection_refuses_a_fast_run_it_cannot_afford(self) -> None:
        profile = resolve(PROFILES, DEFAULT_PROFILE)
        episode_count = profile.episode_count(len(MODES))
        guard = CostGuard(ceiling_usd=1.0)
        with self.assertRaises(BudgetExceeded) as raised:
            guard.check_projection(episode_count)
        self.assertIn("21 episodes", str(raised.exception))

    def test_a_fast_sweep_stops_scheduling_at_the_ceiling_like_any_other(self) -> None:
        output = self.root / "fast-halts"
        run_config = config(
            output=output,
            iteration_profile=DEFAULT_PROFILE,
            instance_ids=TINY_IDS,
            ceiling=8.0,
        )
        result, _ = replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=scripted_sweep(
                suite=self.suite, eval_run_id=output.name, cost_per_episode=3.0
            ),
            run_config=run_config,
        )
        self.assertEqual(result.manifest["budget"]["halt_reason"], HALT_CEILING)
        self.assertEqual(len(read_jsonl(result.run_dir / "episodes.jsonl")), 3)


class AFastRunDirectoryIsTellableFromAMeasuredOne(unittest.TestCase):
    """By reading the manifest alone, not by knowing which command produced it."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.suite = tiny_suite()

    def sweep(self, *, name: str, **kwargs):
        output = self.root / name
        run_config = config(output=output, **kwargs)
        return replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=scripted_sweep(
                suite=self.suite,
                eval_run_id=output.name,
                resolved=[(TINY_IDS[0], "curated")],
            ),
            run_config=run_config,
        )

    def test_the_manifest_alone_names_the_profile_and_the_label(self) -> None:
        # Recorded as a subprocess run while the kernel call is replayed: this
        # is what a measured fast run would write, which is the thing under
        # test. Only the process seam is substituted.
        result, _ = self.sweep(
            name="fast", process_runner=SUBPROCESS, iteration_profile=DEFAULT_PROFILE
        )
        body = json.loads((result.run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(body["config"]["iteration_profile"], DEFAULT_PROFILE)
        self.assertEqual(body["config"]["not_evidence"], CONCLUSION_ITERATION)

    def test_a_measured_manifest_says_neither(self) -> None:
        result, _ = self.sweep(name="measured", process_runner=SUBPROCESS)
        body = json.loads((result.run_dir / "manifest.json").read_text(encoding="utf-8"))
        self.assertIsNone(body["config"]["iteration_profile"])
        self.assertIsNone(body["config"]["not_evidence"])

    def test_a_fast_run_directory_recomputes_byte_for_byte(self) -> None:
        result, _ = self.sweep(
            name="fast-recompute", process_runner=SUBPROCESS, iteration_profile=DEFAULT_PROFILE
        )
        summary, rendered, matches, drifted = pack_recompute(result.run_dir)
        self.assertTrue(matches)
        self.assertEqual(drifted, [])
        self.assertEqual(summary["conclusion"], CONCLUSION_ITERATION)
        self.assertEqual(
            rendered, (result.run_dir / "summary.json").read_text(encoding="utf-8")
        )

    def test_the_summary_on_disk_carries_the_label_on_every_cell(self) -> None:
        result, _ = self.sweep(
            name="fast-cells", process_runner=SUBPROCESS, iteration_profile=DEFAULT_PROFILE
        )
        body = json.loads((result.run_dir / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(body["not_evidence"], CONCLUSION_ITERATION)
        for cell in body["cells"]:
            self.assertEqual(cell["not_evidence"], CONCLUSION_ITERATION)


class ThePinnedProfileDrivesAWholeSweep(unittest.TestCase):
    """The seven pinned ids, the three real modes, the real orchestrator.

    The instance *rows* are synthetic even though the ids are the pinned ones.
    :func:`~evals.packs.swebench_multilingual.replay_script.build` matches a command
    by substring and the agent's argv carries the issue text verbatim, so a
    replay over real problem statements can have a step like ``("start", name)``
    answer the agent's own command — ``nushell__nushell-12901`` does exactly
    that. That is a limit of the rehearsal harness, not of the profile, and the
    schedule this test cares about is a pure function of the ids.
    """

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)
        self.profile = resolve(PROFILES, DEFAULT_PROFILE)
        self.suite = tiny_suite(self.profile.instance_ids)

    def test_the_real_suite_yields_the_pinned_schedule_in_a_stable_order(self) -> None:
        real = Suite.load()
        scheduled = [
            (instance.instance_id, mode, repetition)
            for instance, mode, repetition in plan_order(
                instances=real.select(self.profile.instance_ids),
                modes=MODES,
                repetitions=self.profile.repetitions,
                order_seed=1729,
            )
        ]
        self.assertEqual(len(scheduled), self.profile.episode_count(len(MODES)))
        self.assertEqual(len(set(scheduled)), len(scheduled))
        self.assertEqual(
            [instance for instance, _, _ in scheduled][::3], list(self.profile.instance_ids)
        )

    def test_it_schedules_exactly_the_episodes_its_projection_priced(self) -> None:
        output = self.root / "rust-smoke-7"
        run_config = config(
            output=output,
            instance_ids=self.profile.instance_ids,
            iteration_profile=self.profile.name,
            episode_timeout_seconds=self.profile.episode_timeout_seconds,
            process_runner=SUBPROCESS,
        )
        result, _ = replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=scripted_sweep(
                suite=self.suite, eval_run_id=output.name, cost_per_episode=0.5
            ),
            run_config=run_config,
        )
        self.assertEqual(
            len(read_jsonl(result.run_dir / "episodes.jsonl")),
            self.profile.episode_count(len(MODES)),
        )
        self.assertEqual(
            result.manifest["episodes_planned"], self.profile.episode_count(len(MODES))
        )
        self.assertEqual(result.summary["conclusion"], CONCLUSION_ITERATION)
        self.assertIn(
            "7 of 43 pinned instances, 1 repetition, a 600-second episode timeout",
            result.summary["conclusion_note"],
        )
        # Every cell reports counts over the fixed denominator and no rate.
        for cell in result.summary["cells"]:
            self.assertEqual(cell["attempted"], len(self.profile.instance_ids))
            self.assertEqual(cell["denominator"], SUITE_DENOMINATOR)
            self.assertIsNone(cell["resolved_rate"])
            self.assertEqual(cell["not_evidence"], CONCLUSION_ITERATION)

    def test_the_profile_exercises_the_composition_axis_and_stays_labelled_iteration(self) -> None:
        # The profile only ever narrows instances/repetitions/timeout; it must
        # compose freely with whichever axis a run varies, and never launder
        # a cheap sweep's numbers into something a reader could mistake for
        # evidence just because a real axis (rather than plain graph search)
        # produced them.
        policies = ("deterministic", "model")
        output = self.root / "rust-smoke-7-composition"
        run_config = config(
            output=output,
            modes=policies,
            instance_ids=self.profile.instance_ids,
            iteration_profile=self.profile.name,
            episode_timeout_seconds=self.profile.episode_timeout_seconds,
            process_runner=SUBPROCESS,
            topology_work=True,
            axis=AXIS_COMPOSITION,
            graph_search_mode="both",
        )
        result, _ = replay_sweep(
            tmp=output,
            suite=self.suite,
            episodes=scripted_sweep(
                suite=self.suite, eval_run_id=output.name, modes=policies, cost_per_episode=0.5
            ),
            run_config=run_config,
        )
        self.assertEqual(
            len(read_jsonl(result.run_dir / "episodes.jsonl")),
            self.profile.episode_count(len(policies)),
        )
        self.assertEqual(
            result.manifest["episodes_planned"], self.profile.episode_count(len(policies))
        )
        self.assertEqual(result.manifest["config"]["axis"], AXIS_COMPOSITION)
        # Projection/cost were visible before execution: the manifest carries
        # the same recorded ceiling and per-episode planning rate any other
        # sweep does, unaffected by which axis is varying.
        self.assertEqual(
            result.manifest["config"]["planning_cost_per_episode_usd"],
            PLANNING_COST_PER_EPISODE_USD,
        )
        self.assertEqual(result.summary["conclusion"], CONCLUSION_ITERATION)
        for cell in result.summary["cells"]:
            self.assertEqual(cell["attempted"], len(self.profile.instance_ids))
            self.assertEqual(cell["denominator"], SUITE_DENOMINATOR)
            self.assertIsNone(cell["resolved_rate"])
            self.assertEqual(cell["not_evidence"], CONCLUSION_ITERATION)


class TheCommandResolvesAProfileAndRefusesToBlurIt(unittest.TestCase):
    """One run command, differing only in recorded configuration."""

    def setUp(self) -> None:
        holder = tempfile.TemporaryDirectory()
        self.addCleanup(holder.cleanup)
        self.root = Path(holder.name)

    def invoke(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = cli.main(SwebenchMultilingualPack(), argv)
        return code, out.getvalue(), err.getvalue()

    def base(self, name: str) -> list[str]:
        return [
            "run",
            "--output",
            str(self.root / name),
            "--polaris-code",
            "/nonexistent/polaris-code",
        ]

    def test_the_profile_is_offered_by_name(self) -> None:
        parser = cli.build_parser(SwebenchMultilingualPack())
        args = parser.parse_args(self.base("x") + ["--iteration-profile", DEFAULT_PROFILE])
        self.assertEqual(args.iteration_profile, DEFAULT_PROFILE)

    def test_hand_picked_instances_alongside_a_profile_are_a_usage_error(self) -> None:
        # Otherwise the pinned subset is not pinned and two people's cheap runs
        # stop being comparable, which is the whole reason it is named.
        code, _, err = self.invoke(
            self.base("clash")
            + ["--iteration-profile", DEFAULT_PROFILE, "--instances", "astral-sh__ruff-15309"]
        )
        self.assertEqual(code, 2)
        self.assertIn("--instances", err)
        self.assertFalse((self.root / "clash").exists())

    def test_repetitions_alongside_a_profile_are_a_usage_error(self) -> None:
        code, _, err = self.invoke(
            self.base("clash2") + ["--iteration-profile", DEFAULT_PROFILE, "--repetitions", "3"]
        )
        self.assertEqual(code, 2)
        self.assertIn("--repetitions", err)

    def captured_config(self, extra: list[str]):
        """The `RunConfig` the command would hand the orchestrator.

        Patched at the seam rather than run, because what is under test is the
        assembly: a ``--iteration-profile`` that reached the parser but not the
        configuration would produce a full-suite run nobody asked to pay for,
        recorded as a measurement.
        """
        with mock.patch("evals.core.cli.run_sweep") as sweep:
            sweep.side_effect = RunDirectoryExists("stopped before spending anything")
            self.invoke(self.base("assembled") + extra)
        self.assertTrue(sweep.called)
        return sweep.call_args.kwargs["config"]

    def test_the_profile_reaches_the_recorded_configuration(self) -> None:
        profile = resolve(PROFILES, DEFAULT_PROFILE)
        recorded = self.captured_config(["--iteration-profile", DEFAULT_PROFILE])
        self.assertEqual(recorded.iteration_profile, DEFAULT_PROFILE)
        self.assertEqual(recorded.instance_ids, profile.instance_ids)
        self.assertEqual(recorded.repetitions, profile.repetitions)
        self.assertEqual(recorded.episode_timeout_seconds, profile.episode_timeout_seconds)
        self.assertEqual(recorded.modes, MODES)
        self.assertEqual(recorded.to_json()["not_evidence"], CONCLUSION_ITERATION)

    def test_a_run_without_a_profile_keeps_the_documented_defaults(self) -> None:
        recorded = self.captured_config([])
        self.assertIsNone(recorded.iteration_profile)
        self.assertEqual(recorded.instance_ids, ())
        self.assertEqual(recorded.repetitions, 1)
        self.assertEqual(recorded.episode_timeout_seconds, 1800)
        self.assertIsNone(recorded.to_json()["not_evidence"])

    def test_a_stated_zero_repetition_count_is_still_refused(self) -> None:
        # Resolving the default must not swallow a value the caller stated.
        code, _, err = self.invoke(self.base("zero") + ["--repetitions", "0"])
        self.assertEqual(code, 2)
        self.assertIn("--repetitions must be at least 1", err)

    def test_the_documented_full_sweep_flags_still_work_without_a_profile(self) -> None:
        parser = cli.build_parser(SwebenchMultilingualPack())
        args = parser.parse_args(
            self.base("full") + ["--repetitions", "1", "--episode-timeout-seconds", "1800"]
        )
        self.assertIsNone(args.iteration_profile)
        self.assertEqual(args.repetitions, 1)
        self.assertEqual(args.episode_timeout_seconds, 1800)


class OlderManifestCompatibility(unittest.TestCase):
    """Manifests that lack a newer field still collate."""

    def test_a_manifest_written_before_profiles_existed_still_collates(self) -> None:
        # A manifest without an `iteration_profile` key must be read as
        # "measured", not as a crash.
        stale = manifest(process_runner=SUBPROCESS)
        del stale["config"]["process_runner"]
        del stale["config"]["iteration_profile"]
        del stale["config"]["instance_ids"]
        del stale["config"]["episode_timeout_seconds"]
        summary = summarise(manifest=stale, episodes=episodes(), grader_cells=cells({}))
        self.assertIsNone(summary["iteration_profile"])
        self.assertIsNone(summary["not_evidence"])


class TheProfileRegistryIsWellFormed(unittest.TestCase):
    """Whatever profiles exist, none of them may be a second runner."""

    def test_every_registered_profile_is_named_after_its_key(self) -> None:
        for name, profile in PROFILES.items():
            with self.subTest(name=name):
                self.assertEqual(profile.name, name)
                self.assertTrue(profile.instance_ids)
                self.assertGreaterEqual(profile.repetitions, 1)
                self.assertGreater(profile.episode_timeout_seconds, 0)

    def test_every_registered_profile_is_cheaper_than_a_measured_sweep(self) -> None:
        for name, profile in PROFILES.items():
            with self.subTest(name=name):
                self.assertLess(profile.episode_count(len(MODES)), SUITE_DENOMINATOR * len(MODES))

    def test_no_profile_may_claim_to_clear_the_bar_by_repeating_itself(self) -> None:
        # Three repetitions is one of the four bar conditions; a profile that
        # set it would look like it had met one, so none does.
        for name, profile in PROFILES.items():
            with self.subTest(name=name):
                self.assertLess(profile.repetitions, EVIDENCE_BAR.min_sweeps)


if __name__ == "__main__":
    unittest.main()
