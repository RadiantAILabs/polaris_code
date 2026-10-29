"""The SWE-bench Verified dataset pack.

Verified is a single Python-only, 500-instance official SWE-bench dataset. It
is a narrower use of the existing harness than Multilingual, so this module
only supplies dataset facts and adapters; it does not change the shared core.
"""

from __future__ import annotations

import argparse
import hashlib
import os
import shutil
import sys
import urllib.request
from pathlib import Path
from typing import Any, Mapping, Sequence

from ...core.config import AXIS_COMPOSITION, AXIS_TOPOLOGY, RunConfig
from ...core.pack import DatasetPack, EnvironmentProvider, GraderAdapter, Suite as SuiteProtocol
from ...core.process import Completed, ProcessRunner, SubprocessRunner
from ...core.profiles import IterationProfile
from .environment import EpisodeRunner
from .grader import GRADER_MODULE, MERGED_PREDICTIONS, Grader
from .policy import policy
from .suite import SUITE_DENOMINATOR, Suite, SuiteError

#: Verified requires an explicit per-run spending ceiling; a normal invocation
#: starts at zero rather than inheriting another dataset's default.
DEFAULT_API_COST_CEILING_USD = 0.0
#: The official evaluator's documented free-disk prerequisite.
REQUIRED_FREE_GIB = 120

#: Which `cosmos::domains` variant directory an arm's notes come from. An arm
#: with no entry here (`generic`) reads no domain notes at all.
_NOTES_VARIANT_BY_ARM = {
    "generic-notes": "expert",
    "expert": "expert",
    "composed": "expert",
    "experience": "experience",
}


def notes_digest(domain: str, arm: str) -> str | None:
    """The digest of the notes text `arm` reads for `domain`, or `None`.

    Reads `crates/polaris_code/src/cosmos/domains/<domain>/<variant>/notes.md`
    straight from this checkout rather than from the agent's own report: the
    notes are public source text compiled into the binary via `include_str!`,
    so hashing the checked-in file is the same identity the binary itself
    would compute, without needing the binary to report it back. `None` for
    `domain="none"` or `arm="generic"`, which read no domain notes at all.
    """
    variant = _NOTES_VARIANT_BY_ARM.get(arm)
    if variant is None or domain == "none":
        return None
    root = Path(__file__).resolve().parents[3]
    notes = root / "crates" / "polaris_code" / "src" / "cosmos" / "domains" / domain / variant / "notes.md"
    return f"sha256:{hashlib.sha256(notes.read_bytes()).hexdigest()}"


class SwebenchVerifiedPack(DatasetPack):
    """The official 500-instance human-validated SWE-bench Verified pack."""

    name = "swebench-verified"
    version = "1.0.0"
    refusals = (SuiteError,)

    def load_suite(self) -> Suite:
        """Read the locked official dataset rows."""
        return Suite.load()

    def policy(self) -> dict[str, Any]:
        """Return the dataset-specific split and redistribution facts."""
        return policy()

    def default_api_cost_ceiling_usd(self) -> float:
        """Require an explicit spending ceiling before any agent episode."""
        return DEFAULT_API_COST_CEILING_USD

    def summary_suite_keys(self) -> tuple[str, ...]:
        """Return the pins a Verified result repeats beside its rate."""
        return (
            "dataset",
            "dataset_revision",
            "grader_revision",
            "denominator",
            "instances_sha256",
        )

    def digested_outputs(self) -> tuple[str, ...]:
        """Return the pack-specific prediction artifact."""
        return (MERGED_PREDICTIONS,)

    def iteration_profiles(self) -> Mapping[str, IterationProfile]:
        """Verified deliberately has no paid iteration profile by default."""
        return {}

    def environment(
        self,
        *,
        config: RunConfig,
        suite: SuiteProtocol,
        process_runner: ProcessRunner,
        api_key: str | None,
        options: argparse.Namespace | None = None,
    ) -> EnvironmentProvider:
        """Return one disposable official container per agent episode."""
        # Only the *names* `config.lean_env_names` recorded are read back
        # here; a name present in `config` but no longer set on this host
        # (e.g. a manifest recomputed elsewhere) is simply omitted rather
        # than forwarded as empty.
        lean_env = {
            name: value
            for name in config.lean_env_names
            if (value := os.environ.get(name)) is not None
        }
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
            domain=config.domain,
            arm=config.arm,
            notes_digest=notes_digest(config.domain, config.arm),
            lean_env=lean_env,
        )

    def grader(
        self,
        *,
        config: RunConfig,
        suite: SuiteProtocol,
        process_runner: ProcessRunner,
        options: argparse.Namespace | None = None,
    ) -> GraderAdapter:
        """Return the official evaluator adapter for one matrix cell."""
        return Grader(
            process_runner,
            dataset=getattr(suite, "dataset"),
            max_workers=config.max_workers,
            timeout=config.grade_timeout_seconds,
            python=getattr(options, "grader_python", None) or "python",
        )

    def add_run_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Add assertions for every globally meaningful Verified pin."""
        parser.add_argument("--dataset", default=None, help="assert the pinned dataset name")
        parser.add_argument("--dataset-revision", default=None, help="assert the pinned dataset revision")
        parser.add_argument("--grader-revision", default=None, help="assert the pinned grader revision")
        parser.add_argument("--language", default="Python", help="assert the pinned suite language")
        parser.add_argument(
            "--grader-python",
            default="python",
            help="interpreter that has the pinned SWE-bench grader installed",
        )

    def add_preflight_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Add the interpreter whose installed grader preflight checks."""
        parser.add_argument(
            "--grader-python",
            default="python",
            help="interpreter that has the pinned SWE-bench grader installed",
        )

    def check_run_arguments(
        self, options: argparse.Namespace, suite: SuiteProtocol
    ) -> str | None:
        """Refuse a command that claims a pin other than the checked-in one."""
        for flag, expected, actual in (
            ("--dataset", options.dataset, suite.dataset),  # type: ignore[attr-defined]
            ("--dataset-revision", options.dataset_revision, suite.dataset_revision),  # type: ignore[attr-defined]
            ("--grader-revision", options.grader_revision, suite.grader_revision),  # type: ignore[attr-defined]
            ("--language", options.language, suite.language),  # type: ignore[attr-defined]
        ):
            if expected is not None and expected != actual:
                return f"{flag}={expected!r} does not match the pinned suite ({actual!r})"
        return None

    def preflight_checks(
        self, options: argparse.Namespace, suite: SuiteProtocol
    ) -> list[tuple[str, bool, str]]:
        """Return the non-agent prerequisites for an official run."""
        grader_python = shutil.which(getattr(options, "grader_python", "python"))
        checks: list[tuple[str, bool, str]] = [
            (
                "grader interpreter",
                grader_python is not None,
                grader_python or f"{getattr(options, 'grader_python', 'python')} not on PATH",
            )
        ]
        if grader_python:
            found = SubprocessRunner().run(
                [grader_python, "-c", f"import {GRADER_MODULE.split('.')[0]}"]
            )
            checks.append(("official grader package", found.ok, GRADER_MODULE))
        free_gib = shutil.disk_usage(Path.cwd()).free / (1 << 30)
        checks.extend(
            [
                (
                    f"free disk >= {REQUIRED_FREE_GIB} GiB",
                    free_gib >= REQUIRED_FREE_GIB,
                    f"{free_gib:.1f} GiB",
                ),
                (
                    "pinned suite",
                    len(suite.instance_ids) == SUITE_DENOMINATOR,
                    f"{len(suite.instance_ids)} instances @ {suite.dataset_revision[:12]}",  # type: ignore[attr-defined]
                ),
            ]
        )
        return checks

    def add_commands(self, commands: Any) -> None:
        """Add pin verification and a no-agent official gold-grade command."""
        pin = commands.add_parser("pin", help="verify the checked-in suite against its upstream revision")
        pin.add_argument("--offline", action="store_true", help="validate the lock without contacting the network")
        gold = commands.add_parser("gold", help="grade the checked-in upstream reference patch without an agent")
        gold.add_argument("--output", required=True, help="fresh directory for official evaluator output")
        gold.add_argument("--grader-python", default="python", help="interpreter with the pinned official grader")
        gold.add_argument("--max-workers", type=int, default=1)
        gold.add_argument("--grade-timeout-seconds", type=int, default=1800)

    def handle_command(
        self, command: str, options: argparse.Namespace, suite: SuiteProtocol
    ) -> int:
        """Run one of the dataset-local commands."""
        if command == "pin":
            return _pin(options, suite)  # type: ignore[arg-type]
        if command == "gold":
            return _gold(options, suite)  # type: ignore[arg-type]
        return super().handle_command(command, options, suite)


def _pin(options: argparse.Namespace, suite: Suite) -> int:
    """Validate the upstream parquet content hash against the checked-in lock."""
    print(f"dataset          {suite.dataset}")
    print(f"dataset revision {suite.dataset_revision}")
    print(f"dataset file     {suite.dataset_file} ({suite.dataset_file_bytes} bytes)")
    print(f"file sha256      {suite.dataset_file_sha256}")
    print(f"grader           {suite.grader_repo}@{suite.grader_revision}")
    print(f"instances        {len(suite.instances)} (digest {suite.content_digest()})")
    print(f"gold             {suite.gold.instance_id}")
    if options.offline:
        return 0
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
        print(f"error: upstream dataset file has drifted: {len(body)} bytes / {digest}", file=sys.stderr)
        return 1
    print("\nupstream dataset file matches the checked-in pin")
    return 0


def _gold(options: argparse.Namespace, suite: Suite) -> int:
    """Ask the pinned official evaluator to score its upstream reference patch."""
    output = Path(options.output)
    if output.exists() and any(output.iterdir()):
        print(f"error: gold output is not empty: {output}", file=sys.stderr)
        return 2
    output.mkdir(parents=True, exist_ok=True)
    # ``run_sweep`` creates this shared adapter work directory before a normal
    # cell grade.  ``gold`` bypasses the sweep by design, so it owns that one
    # setup step while still delegating every correctness decision to Grader.
    (output / "grader").mkdir()
    grader = Grader(
        _GoldRunner(SubprocessRunner(), output / "hf-cache"),
        dataset=suite.dataset,
        max_workers=options.max_workers,
        timeout=options.grade_timeout_seconds,
        python=options.grader_python,
    )
    report = grader.grade(
        eval_run_id="verified-gold",
        mode="gold",
        repetition=1,
        patches={suite.gold.instance_id: suite.gold.model_patch},
        instance_ids=[suite.gold.instance_id],
        run_dir=output,
    )
    grader.write_merged_predictions(output)
    if report.graded and suite.gold.instance_id in report.resolved_ids:
        print(f"PASS official grader resolved {suite.gold.instance_id}")
        return 0
    detail = report.note or "official grader did not resolve the gold patch"
    print(f"FAIL {suite.gold.instance_id}: {detail}", file=sys.stderr)
    return 1


class _GoldRunner:
    """Give the direct evaluator command a writable, disposable HF cache."""

    kind = "subprocess"

    def __init__(self, delegate: SubprocessRunner, cache: Path) -> None:
        self._delegate = delegate
        self._cache = cache.resolve()

    def run(
        self,
        argv: Sequence[str],
        *,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        """Run with the caller's environment plus an output-local HF cache."""
        merged = dict(os.environ)
        if env is not None:
            merged.update(env)
        merged["HF_HOME"] = str(self._cache)
        return self._delegate.run(argv, env=merged, timeout=timeout, cwd=cwd)


__all__ = [
    "DEFAULT_API_COST_CEILING_USD",
    "REQUIRED_FREE_GIB",
    "SwebenchVerifiedPack",
    "SuiteError",
    "notes_digest",
]
