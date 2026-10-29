"""Named fast configurations: a signal worth acting on, never a result.

The three-repetition, whole-suite bar in :mod:`evals.core.config` is a
publication bar. It is the right bar for a claim and the wrong one for
iteration: repeatedly running full sweeps is expensive. So a team will iterate
against *something*, and the
risk being managed here is not slowness — it is a cheap number escaping with a
credible-looking label and being repeated until it becomes a fact.

An *iteration profile* is that something, made honest:

* it is **named, pinned and checked in**, so two people's fast runs attempted
  the same tasks in the same order and are comparable to each other;
* it goes through the **same** ``run`` command and the same orchestrator as a
  measured sweep, differing only in recorded configuration — the way the
  replayed plumbing runs differ only by a process seam;
* its name is recorded in ``manifest.json``, which makes
  :func:`evals.core.config.not_evidence_label` answer
  :data:`~evals.core.config.CONCLUSION_ITERATION` and
  :mod:`evals.core.collate` report that conclusion **whatever the numbers
  say**; and
* it gets no ceiling of its own. A cheap mode is still a mode that spends money,
  so :mod:`evals.core.budget` governs it unchanged.

The machinery is here; the profiles themselves are a pack's, because a pinned
subset names instances only one dataset has. A pack publishes its profiles from
:meth:`~evals.core.pack.DatasetPack.iteration_profiles`, and the shared ``run``
command offers exactly those.

What such a run can tell you
----------------------------

It answers one question: *does the agent still do the job, in every graph-search
mode, on every repository the profile covers?* A patch that stopped applying, a
mode whose graph tools error out, a container step that broke, a prompt change
that made the agent stop editing — those show up as empty patches or failed
turns across a handful of instances, and they show up for a fraction of a
sweep's cost.

What it provably cannot tell you
--------------------------------

Which mode resolves more tasks. A handful of instances and one repetition clear
none of the four evidence-bar conditions and no arithmetic over them ever will:
the sampling noise is larger than the effect the bar was written to detect. A
shortened episode timeout adds a second blindness in the same direction — an
instance a measured sweep would have finished is recorded here as a timeout, so
a profile over-reports timeouts and under-reports resolutions by construction.

What the label is not
---------------------

``incomplete`` is what a run that *meant* to be complete reports when something
stopped it: a halted ceiling, an ungradable cell, a crash. Its remedy is to run
it again properly. An iteration run is partial by construction and re-running it
changes nothing, so reporting it as ``incomplete`` would invite exactly the
wrong repair.

``plumbing-demonstration`` is what a replayed run reports: nothing executed and
every number is a fixture. An iteration run really launches the agent, really
spends money, and its numbers are real observations — of a small, deliberately
unrepresentative sample. Reporting a replayed iteration run as an iteration
signal would present fixtures as observations, which is the larger of the two
lies, so the replay label wins when both apply.

``not distinguished at this budget`` is a statement about a *complete measured
sweep* that fell short of the bar. An iteration run never reaches the point
where that sentence means anything.
"""

from __future__ import annotations

import dataclasses
from typing import Mapping

from .config import CONCLUSION_ITERATION  # noqa: F401 - the label this module exists to earn


@dataclasses.dataclass(frozen=True)
class IterationProfile:
    """One pinned fast configuration.

    Deliberately small. A profile may only narrow what a run attempts; it holds
    no ceiling, no model, no mode list and no grading timeout, because each of
    those is either a spend control that must stay uniform or a dimension the
    caller is usually iterating *on*.
    """

    #: The name the manifest records and the command line spells.
    name: str
    #: The pinned subset, in the suite's own ascending instance-id order.
    instance_ids: tuple[str, ...]
    #: Complete passes. Never three: that is one of the bar's four conditions.
    repetitions: int
    #: Per-episode inference timeout. Shorter than a measured sweep's, which is
    #: what bounds the wall clock and also what biases the failure mix.
    episode_timeout_seconds: int

    def episode_count(self, mode_count: int) -> int:
        """Episodes this profile schedules across ``mode_count`` modes."""
        return len(self.instance_ids) * mode_count * self.repetitions


def profile_names(profiles: Mapping[str, IterationProfile]) -> tuple[str, ...]:
    """``profiles`` keyed in a stable order, for a command's own help text."""
    return tuple(sorted(profiles))


def resolve(profiles: Mapping[str, IterationProfile], name: str) -> IterationProfile:
    """The profile called ``name`` among ``profiles``.

    Raises:
        ValueError: if no such profile is registered. The message names the ones
            that are, because an unrecognised profile is a typo far more often
            than it is a missing feature, and a silently ignored ``--iteration-profile``
            would produce a full-suite run nobody asked to pay for.
    """
    try:
        return profiles[name]
    except KeyError:
        raise ValueError(
            f"unknown iteration profile {name!r}: expected one of "
            f"{', '.join(profile_names(profiles)) or 'none'}"
        ) from None


def iteration_note(
    *,
    name: str,
    selected: int,
    denominator: int,
    repetitions: int,
    episode_timeout_seconds: int | None,
) -> str:
    """The sentence a fast run's summary carries, from its own recorded numbers.

    Every value comes from the manifest under collation rather than from a
    pack's profile table, for two reasons. ``recompute`` compares bytes, so a
    summary whose prose was read out of the current table would drift the day
    that table was edited; and a run recorded by a build that no longer knows
    the profile's name must still be able to explain itself.
    """
    timeout = (
        "" if episode_timeout_seconds is None else f", a {episode_timeout_seconds}-second episode timeout"
    )
    passes = "repetition" if repetitions == 1 else "repetitions"
    return (
        f"NOT EVIDENCE. Fast iteration profile {name!r}: {selected} of {denominator} pinned "
        f"instances, {repetitions} {passes}{timeout}. Real episodes ran and real money was "
        "spent, but this configuration meets none of the four evidence-bar conditions and no "
        "arithmetic over it ever can. It tells you whether the agent still produces a patch in "
        "each graph-search mode; it does not tell you which mode resolves more tasks. See "
        "docs/harness-eval.md."
    )
