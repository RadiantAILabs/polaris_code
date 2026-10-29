"""The SWE-bench Multilingual dataset pack: the first, and the pattern.

Everything in this module is true of this dataset and of nothing else — the
pinned 43-instance Rust slice, its ``/testbed`` images, the official SWE-bench
grader, and the split it was cut from. The core harness holds the parts that
would be identical for any dataset.
"""

from __future__ import annotations

import argparse
import shutil
import sys
from pathlib import Path
from typing import Any, Mapping

from ...core.config import AXIS_COMPOSITION, AXIS_TOPOLOGY, RunConfig
from ...core.pack import DatasetPack, EnvironmentProvider, GraderAdapter, Suite as SuiteProtocol
from ...core.process import ProcessRunner, SubprocessRunner
from ...core.profiles import IterationProfile
from .environment import EpisodeRunner
from .grader import GRADER_MODULE, MERGED_PREDICTIONS, Grader
from .profiles import PROFILES
from .suite import SUITE_DENOMINATOR, Suite, SuiteError

#: Require callers to choose a spending ceiling for each run.
DEFAULT_API_COST_CEILING_USD = 0.0

#: Free disk the official Docker grader needs for this slice, in gibibytes.
REQUIRED_FREE_GIB = 120


class SwebenchMultilingualPack(DatasetPack):
    """SWE-bench Multilingual, restricted to its pinned Rust slice."""

    name = "swebench-multilingual"
    version = "1.1.0"
    refusals = (SuiteError,)

    # -- the dataset ------------------------------------------------------

    def load_suite(self) -> Suite:
        """Read and validate ``suite/instances.json``."""
        return Suite.load()

    def policy(self) -> dict[str, Any]:
        """The split this slice was cut from, and its redistribution terms.

        ``problem_statement_licence`` is ``null`` on purpose. The 43 problem
        statements are checked into this repository so an episode's prompt is
        auditable next to its result, and nobody has yet established the terms
        under which the upstream dataset licenses that text. ``null`` records
        that gap; writing a licence name here that nobody verified would be the
        one thing worse than leaving it open.
        """
        return {
            "split": "test",
            "split_is_the_only_one_used": True,
            "language_filter": "Rust",
            "held_out_split": None,
            "problem_statement_checked_in": True,
            "problem_statement_licence": None,
            "problem_statement_licence_note": (
                "Refer to the upstream dataset for problem-text redistribution "
                "terms."
            ),
            "images_are_upstream": True,
            "grading_is_official": True,
        }

    def default_api_cost_ceiling_usd(self) -> float:
        """The default spending ceiling for a sweep of this dataset."""
        return DEFAULT_API_COST_CEILING_USD

    def summary_suite_keys(self) -> tuple[str, ...]:
        """What a reader of this dataset's summary needs to see beside a rate.

        The dataset, the two revisions that decide what was asked and how it
        was judged, the fixed denominator, and the content digest of the
        instance list. Nothing else: the summary is a derived file whose bytes
        are compared, so its shape is part of the evidence.
        """
        return (
            "dataset",
            "dataset_revision",
            "grader_revision",
            "denominator",
            "instances_sha256",
        )

    def digested_outputs(self) -> tuple[str, ...]:
        """The merged prediction file, which is this pack's own artifact."""
        return (MERGED_PREDICTIONS,)

    def iteration_profiles(self) -> Mapping[str, IterationProfile]:
        """This pack's pinned fast subsets (currently ``rust-smoke-7``)."""
        return PROFILES

    # -- the providers ----------------------------------------------------

    def environment(
        self,
        *,
        config: RunConfig,
        suite: SuiteProtocol,
        process_runner: ProcessRunner,
        api_key: str | None,
        options: argparse.Namespace | None = None,
    ) -> EnvironmentProvider:
        """One disposable container per episode, from the instance's image."""
        return EpisodeRunner(
            process_runner,
            container_runtime=config.container_runtime,
            host_binary=config.polaris_code_binary,
            episode_timeout_seconds=config.episode_timeout_seconds,
            allow_edit=config.allow_edit,
            allow_command=config.allow_command,
            topology_work=config.topology_work,
            topology_axis=config.axis == AXIS_TOPOLOGY,
            composition_axis=config.axis == AXIS_COMPOSITION,
            graph_search_mode=config.graph_search_mode,
            api_key=api_key,
            composition_policy=config.composition_policy,
            topology_intent=config.topology_intent,
            max_turns=config.max_turns,
            max_output_tokens=config.max_output_tokens,
            keep_transcripts=config.keep_transcripts,
        )

    def grader(
        self,
        *,
        config: RunConfig,
        suite: SuiteProtocol,
        process_runner: ProcessRunner,
        options: argparse.Namespace | None = None,
    ) -> GraderAdapter:
        """The official grader, one invocation per matrix cell."""
        dataset = getattr(suite, "dataset")
        return Grader(
            process_runner,
            dataset=dataset,
            max_workers=config.max_workers,
            timeout=config.grade_timeout_seconds,
            python=getattr(options, "grader_python", None) or "python",
        )

    # -- the command line -------------------------------------------------

    def add_run_arguments(self, parser: argparse.ArgumentParser) -> None:
        """The pin assertions and the grader interpreter.

        The five revision flags are *asserted*, never applied: they exist so the
        documented command states its pins in full and fails loudly if the
        checked-in suite ever moves underneath it.
        """
        parser.add_argument("--dataset", default=None, help="assert the pinned dataset name")
        parser.add_argument("--dataset-revision", default=None, help="assert the pinned dataset revision")
        parser.add_argument("--task-repo-revision", default=None, help="assert the pinned task-repository revision")
        parser.add_argument("--grader-revision", default=None, help="assert the pinned grader revision")
        parser.add_argument("--language", default="Rust", help="assert the pinned suite language")
        parser.add_argument(
            "--grader-python",
            default="python",
            help="interpreter that has the pinned SWE-bench grader installed",
        )

    def add_preflight_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Only the grader interpreter: it is the one tool preflight looks for."""
        parser.add_argument(
            "--grader-python",
            default="python",
            help="interpreter that has the pinned SWE-bench grader installed",
        )

    def check_run_arguments(
        self, options: argparse.Namespace, suite: SuiteProtocol
    ) -> str | None:
        """Refuse a command whose stated pins disagree with the checked-in one."""
        for flag, expected, actual in (
            ("--dataset", options.dataset, suite.dataset),  # type: ignore[attr-defined]
            ("--dataset-revision", options.dataset_revision, suite.dataset_revision),  # type: ignore[attr-defined]
            ("--task-repo-revision", options.task_repo_revision, suite.task_repo_revision),  # type: ignore[attr-defined]
            ("--grader-revision", options.grader_revision, suite.grader_revision),  # type: ignore[attr-defined]
            ("--language", options.language, suite.language),  # type: ignore[attr-defined]
        ):
            if expected is not None and expected != actual:
                return f"{flag}={expected!r} does not match the pinned suite ({actual!r})"
        return None

    def preflight_checks(
        self, options: argparse.Namespace, suite: SuiteProtocol
    ) -> list[tuple[str, bool, str]]:
        """What this dataset needs beyond the binary and the provider key."""
        checks: list[tuple[str, bool, str]] = []
        grader_python = shutil.which(getattr(options, "grader_python", "python"))
        checks.append(
            (
                "grader interpreter",
                grader_python is not None,
                grader_python or f"{getattr(options, 'grader_python', 'python')} not on PATH",
            )
        )
        if grader_python:
            found = SubprocessRunner().run([grader_python, "-c", f"import {GRADER_MODULE.split('.')[0]}"])
            checks.append(("official grader package", found.ok, GRADER_MODULE))
        free_gib = shutil.disk_usage(Path.cwd()).free / (1 << 30)
        checks.append(
            (f"free disk >= {REQUIRED_FREE_GIB} GiB", free_gib >= REQUIRED_FREE_GIB, f"{free_gib:.1f} GiB")
        )
        checks.append(
            (
                "pinned suite",
                len(suite.instance_ids) == SUITE_DENOMINATOR,
                f"{len(suite.instance_ids)} instances @ {suite.dataset_revision[:12]}",  # type: ignore[attr-defined]
            )
        )
        return checks

    def add_commands(self, commands: Any) -> None:
        """``pin``: prove the checked-in suite still matches upstream."""
        pin = commands.add_parser("pin", help="verify the checked-in suite against its upstream revision")
        pin.add_argument("--offline", action="store_true", help="validate the file without contacting the network")

    def handle_command(
        self, command: str, options: argparse.Namespace, suite: SuiteProtocol
    ) -> int:
        """Run ``pin``."""
        if command != "pin":
            return super().handle_command(command, options, suite)
        return _pin(options, suite)  # type: ignore[arg-type]


def _pin(options: argparse.Namespace, suite: Suite) -> int:
    """The ``pin`` command: prove the checked-in suite still matches upstream."""
    print(f"dataset          {suite.dataset}")
    print(f"dataset revision {suite.dataset_revision}")
    print(f"dataset file     {suite.dataset_file} ({suite.dataset_file_bytes} bytes)")
    print(f"file sha256      {suite.dataset_file_sha256}")
    print(f"task repo        {suite.task_repo}@{suite.task_repo_revision}")
    print(f"grader           {suite.grader_repo}@{suite.grader_revision}")
    print(f"instances        {len(suite.instances)} (digest {suite.content_digest()})")
    if options.offline:
        return 0

    import hashlib
    import urllib.request

    url = (
        f"https://huggingface.co/datasets/{suite.dataset}/resolve/"
        f"{suite.dataset_revision}/{suite.dataset_file}"
    )
    try:
        with urllib.request.urlopen(url, timeout=300) as response:  # noqa: S310 - fixed https host
            body = response.read()
    except OSError as err:
        print(f"error: could not fetch {url}: {err}", file=sys.stderr)
        return 2
    digest = hashlib.sha256(body).hexdigest()
    if digest != suite.dataset_file_sha256 or len(body) != suite.dataset_file_bytes:
        print(
            f"error: upstream dataset file has drifted: {len(body)} bytes / {digest}",
            file=sys.stderr,
        )
        return 1
    print("\nupstream dataset file matches the checked-in pin")
    return 0


__all__ = [
    "DEFAULT_API_COST_CEILING_USD",
    "REQUIRED_FREE_GIB",
    "SwebenchMultilingualPack",
    "SuiteError",
]
