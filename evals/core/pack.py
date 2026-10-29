"""What a dataset pack must provide, and nothing more.

The core harness owns run identity, accounting, the budget guard, the
statistics, the evidence bar, the conclusion labels, immutability and the
replay seam. A *pack* owns one dataset: its suite list, its denominator, the
environment its episodes run in, the grader that decides correctness, and its
split and licence policy.

Adding a second dataset means writing one :class:`DatasetPack` and its four
collaborators. It must not mean editing anything under :mod:`evals.core`.

The contract, in one list
-------------------------

A pack supplies:

1. **Identity** — :attr:`DatasetPack.name` and :attr:`DatasetPack.version`, both
   recorded in every manifest so a run directory names what produced it.
2. **A suite** — :meth:`DatasetPack.load_suite`, returning a :class:`Suite`:
   the pinned instance list, its fixed denominator, and a ``revisions()``
   projection that pins every upstream revision the list was cut from. The
   denominator is the pack's, not the core's; the core only refuses to score a
   cell that did not attempt and grade all of it.
3. **An environment provider** — :meth:`DatasetPack.environment`, returning an
   :class:`EnvironmentProvider` that takes one instance to one
   :class:`~evals.core.episode.EpisodeOutcome`. This is where a container
   image, a working directory and a patch-collection convention live.
4. **A grader adapter** — :meth:`DatasetPack.grader`, returning a
   :class:`GraderAdapter` that turns one cell's patches into a
   :class:`~evals.core.report.CellReport`. Correctness is never the core's to
   decide.
5. **Policy** — :meth:`DatasetPack.policy`, the split the suite was cut from
   and the licence under which its problem text may be redistributed. Recorded
   in the manifest.
6. **Budget default** — :meth:`DatasetPack.default_api_cost_ceiling_usd`, the
   dataset's default safety ceiling. The guard is shared; the number belongs
   to the pack.
7. **Summary projection** — :meth:`DatasetPack.summary_suite_keys`, which keys
   of ``revisions()`` the derived summary repeats. The core copies them through
   verbatim rather than inventing a shape every dataset would have to fit.
8. **Extra digested outputs** — :meth:`DatasetPack.digested_outputs`, any file
   the pack writes into a run directory that must be covered by the manifest's
   content digests.
9. **Iteration profiles** — :meth:`DatasetPack.iteration_profiles`, the named
   fast subsets ``run --iteration-profile`` offers. A profile names instance
   ids, so only a pack can hold one; the refusal that keeps a fast run out of
   evidence is the core's and applies to every pack's profiles alike.
10. **Its own command-line surface** — the pin-assertion flags, the preflight
   checks that need a container runtime or a grader install, and any extra
   subcommand (``pin``, for instance). The core CLI owns ``run``,
   ``recompute`` and the generic half of ``preflight``.

Where a SWE-bench Pro pack attaches
-----------------------------------

``evals/packs/swebench_pro/`` with a :class:`DatasetPack` subclass, registered
in :mod:`evals.packs`, and a ``__main__`` that calls
:func:`evals.core.cli.main` with it. Nothing else.
"""

from __future__ import annotations

import argparse
from pathlib import Path
from typing import Any, Mapping, Protocol, Sequence, runtime_checkable

from .config import RunConfig
from .episode import EpisodeOutcome
from .profiles import IterationProfile
from .report import CellReport


@runtime_checkable
class Instance(Protocol):
    """One unit of benchmark work.

    Two members are all the core reads. Everything else a dataset knows about
    an instance — its image, its base commit, its version — is the pack's, and
    travels only between the pack's own suite and its own environment provider.
    """

    #: Stable identifier. It keys every record, every patch file and every
    #: grader disposition, so it must be unique within a suite and stable
    #: across runs.
    instance_id: str
    #: The problem text, handed to the agent byte-for-byte and never augmented.
    problem_statement: str


@runtime_checkable
class Suite(Protocol):
    """A pinned instance list with a fixed denominator."""

    @property
    def denominator(self) -> int:
        """The fixed denominator of the primary metric.

        Fixed means a subset run, a dropped hard case or a cell that halted on
        the cost ceiling cannot improve a rate — it only makes the cell
        incomplete, which the core then refuses to score.
        """
        ...

    @property
    def instance_ids(self) -> tuple[str, ...]:
        """Every pinned instance id, in the suite's canonical order."""
        ...

    def select(self, instance_ids: Sequence[str]) -> tuple[Instance, ...]:
        """The instances named, in the suite's own order; empty means all."""
        ...

    def revisions(self) -> dict[str, Any]:
        """The manifest projection of everything this suite pins.

        Must include ``denominator``. The core adds ``instance_ids``. Every
        other key is the pack's to choose, and the keys named by
        :meth:`DatasetPack.summary_suite_keys` are repeated in the summary.
        """
        ...


class EnvironmentProvider(Protocol):
    """Takes one instance to one episode outcome, and never raises.

    An environment that cannot be prepared is an *infrastructure failure*; a
    turn that fails, times out or prints unreadable output is a *result*. The
    provider decides which, because only it knows whether the agent was ever
    launched — and the budget guard needs that to tell a known-zero cost from
    an unknown one.
    """

    def run(
        self,
        *,
        instance: Instance,
        mode: str,
        repetition: int,
        attempt: int,
        eval_run_id: str,
        model: str,
    ) -> EpisodeOutcome:
        """Run one attempt and always return an outcome."""
        ...


class GraderAdapter(Protocol):
    """Turns one cell's patches into one official disposition per instance.

    Correctness belongs to the dataset's own grader. An adapter that cannot run
    it returns an *ungraded* :class:`~evals.core.report.CellReport` rather than
    guessing, and the core then reports the cell as incomplete with no score.
    """

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
        """Grade one ``(mode, repetition)`` cell.

        Args:
            patches: the collected patch per attempted instance id.
            instance_ids: the attempted ids, in the suite's canonical order.
            run_dir: the immutable run directory. Any artifact the adapter
                writes must be listed by
                :meth:`DatasetPack.digested_outputs` or live under a directory
                the core already digests.
        """
        ...

    def write_merged_predictions(self, run_dir: Path) -> None:
        """Write the whole-run prediction artifact, if the dataset has one.

        Called once, after every cell has been graded, so the file's byte order
        is a function of the recorded mode matrix rather than of scheduling.
        """
        ...


class DatasetPack:
    """One dataset, as the core harness sees it.

    Subclass and override. The defaults cover the parts a pack may legitimately
    not have: no extra command-line flags, no extra subcommand, no extra
    digested output.
    """

    #: Stable pack name, recorded in every manifest. Kebab-case.
    name: str = ""
    #: Version of this pack's own pinned content and adapters.
    version: str = "0.0.0"
    #: Exceptions this pack raises to *refuse* a command — a suite that will not
    #: load, an instance id that is not pinned. The CLI turns these into a
    #: message and exit code 2; anything else is a bug and keeps its traceback.
    refusals: tuple[type[BaseException], ...] = (RuntimeError,)

    # -- the dataset ------------------------------------------------------

    def load_suite(self) -> Suite:
        """Read and validate the pinned instance list."""
        raise NotImplementedError

    def policy(self) -> dict[str, Any]:
        """The split the suite was cut from and its redistribution licence.

        Recorded in the manifest under ``pack.policy``. A pack that checks
        problem text into this repository must say under what licence it may;
        ``null`` is an admission that nobody has established it, not a claim
        that none is needed.
        """
        raise NotImplementedError

    def default_api_cost_ceiling_usd(self) -> float:
        """The default API-cost ceiling for a sweep of this dataset."""
        raise NotImplementedError

    def summary_suite_keys(self) -> tuple[str, ...]:
        """Keys of ``Suite.revisions()`` the derived summary repeats."""
        raise NotImplementedError

    def digested_outputs(self) -> tuple[str, ...]:
        """Run-directory paths this pack writes that the manifest must digest."""
        return ()

    def iteration_profiles(self) -> Mapping[str, IterationProfile]:
        """The named fast configurations ``run --iteration-profile`` offers.

        A profile pins instance ids, so it can only be a pack's. A pack with
        none offers the flag no choices and every run is a whole-suite run.
        """
        return {}

    # -- the providers ----------------------------------------------------

    def environment(
        self,
        *,
        config: RunConfig,
        suite: Suite,
        process_runner: Any,
        api_key: str | None,
        options: argparse.Namespace | None = None,
    ) -> EnvironmentProvider:
        """Build the environment provider for one sweep.

        ``process_runner`` is the core's replay seam and every command must go
        through it, or a replayed run would execute something real.
        """
        raise NotImplementedError

    def grader(
        self,
        *,
        config: RunConfig,
        suite: Suite,
        process_runner: Any,
        options: argparse.Namespace | None = None,
    ) -> GraderAdapter:
        """Build the grader adapter for one sweep."""
        raise NotImplementedError

    # -- the command line -------------------------------------------------

    def add_run_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Add this pack's own ``run`` flags, such as its revision assertions."""

    def add_preflight_arguments(self, parser: argparse.ArgumentParser) -> None:
        """Add the ``preflight`` flags this pack's own checks read.

        Separate from ``run`` because a preflight neither runs nor spends: the
        revision assertions have nothing to check, and the only flags worth
        offering are the ones that name a tool to look for.
        """

    def check_run_arguments(
        self, options: argparse.Namespace, suite: Suite
    ) -> str | None:
        """Return one error message if the flags disagree with the pins."""
        return None

    def preflight_checks(
        self, options: argparse.Namespace, suite: Suite
    ) -> list[tuple[str, bool, str]]:
        """This pack's prerequisites, as ``(name, satisfied, detail)`` rows."""
        return []

    def add_commands(self, commands: Any) -> None:
        """Add any pack-only subcommand to the core parser."""

    def handle_command(
        self, command: str, options: argparse.Namespace, suite: Suite
    ) -> int:
        """Run a pack-only subcommand and return its exit code."""
        raise NotImplementedError(f"pack {self.name!r} has no command {command!r}")
