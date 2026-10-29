"""``summary.json``: derived, never written by hand, byte-reproducible.

Reads exactly three things — the manifest, ``episodes.jsonl`` and the official
grader dispositions — and derives every number in the summary from them. The
``recompute`` command runs this same function over an existing run directory,
so a summary that cannot be reproduced from its own evidence is a bug that
fails loudly rather than a number that quietly drifts.

Four refusals are built in, and each one exists because the easy alternative
would be a lie:

* **A cell that is not complete has no resolved rate.** Completeness means
  every one of the pack's pinned instances was attempted *and* graded in that
  cell. A subset run, an ungradable cell or a run that halted on the cost
  ceiling therefore reports ``resolved: n`` and ``resolved_rate: null``, never
  ``n / attempted``.
* **A replayed run is never a measurement.** ``process_runner = "replay"``
  forces the conclusion to ``plumbing-demonstration`` whatever the numbers say.
* **A run configured from an iteration profile is never a measurement either.**
  ``iteration_profile`` forces ``iteration-signal-not-evidence`` by the same
  structural route, and for the same reason: see
  :mod:`evals.packs.swebench_multilingual.profiles`.
* **A difference below the evidence bar is "not distinguished".** All four
  conditions — repetitions, five points, consistent direction, interval
  excluding zero — must hold together.

The two structural refusals also close the comparison level, not only the
conclusion. A summary whose headline said ``plumbing-demonstration`` while one
of its ``comparisons`` said ``credible: true`` would hand a reader the
flattering half to quote.
"""

from __future__ import annotations

import dataclasses
import itertools
import json
from typing import Any, Iterable, Mapping, Sequence

from .config import (
    CONCLUSION_CREDIBLE,
    CONCLUSION_INCOMPLETE,
    CONCLUSION_ITERATION,
    CONCLUSION_NOT_DISTINGUISHED,
    CONCLUSION_PLUMBING,
    SUMMARY_SCHEMA,
    EvidenceBar,
    not_evidence_label,
)
from .process import REPLAY
from .profiles import iteration_note
from .report import RESOLVED

#: Keys of the manifest's suite block that the summary repeats when a caller
#: names none. Only the fixed denominator is universal; a pack that wants its
#: upstream revisions echoed in the summary says so with
#: :meth:`~evals.core.pack.DatasetPack.summary_suite_keys`.
DEFAULT_SUMMARY_SUITE_KEYS: tuple[str, ...] = ("denominator",)

__all__ = [
    "CONCLUSION_CREDIBLE",
    "CONCLUSION_INCOMPLETE",
    "CONCLUSION_ITERATION",
    "CONCLUSION_NOT_DISTINGUISHED",
    "CONCLUSION_PLUMBING",
    "Cell",
    "render",
    "summarise",
]


@dataclasses.dataclass(frozen=True)
class Cell:
    """One ``(mode, repetition)`` cell of the matrix."""

    mode: str
    repetition: int
    resolved: int
    attempted: int
    graded: int
    denominator: int
    #: ``None`` unless every pinned instance was attempted and graded.
    resolved_rate: float | None
    measured_cost_usd: float | None
    inference_seconds: float
    complete: bool
    #: The run-level non-evidence label, repeated on every cell. A cell is the
    #: unit that gets copied out on its own, so ``resolved: 4`` has to arrive
    #: carrying the reason it is not a score.
    not_evidence: str | None = None

    def to_json(self) -> dict[str, Any]:
        """The summary projection of this cell."""
        return {
            "mode": self.mode,
            "repetition": self.repetition,
            "resolved": self.resolved,
            "attempted": self.attempted,
            "graded": self.graded,
            "denominator": self.denominator,
            "not_evidence": self.not_evidence,
            "resolved_rate": None if self.resolved_rate is None else round(self.resolved_rate, 6),
            "measured_cost_usd": None if self.measured_cost_usd is None else round(self.measured_cost_usd, 6),
            "inference_seconds": round(self.inference_seconds, 3),
            "complete": self.complete,
        }


def summarise(
    *,
    manifest: Mapping[str, Any],
    episodes: Sequence[Mapping[str, Any]],
    grader_cells: Sequence[Mapping[str, Any]],
    suite_keys: Sequence[str] = DEFAULT_SUMMARY_SUITE_KEYS,
) -> dict[str, Any]:
    """Derive the whole summary from a run directory's own evidence.

    Args:
        suite_keys: which keys of the manifest's suite block to repeat in the
            summary's own ``suite`` member, in the order the pack lists them.
            Copied through verbatim: the core does not know what a dataset
            pins, only that whatever it pinned belongs next to the numbers
            derived from it. A key the manifest does not carry is omitted
            rather than reported as ``null``, so an older run directory
            reproduces under a pack that has since added a pin.
    """
    config = manifest["config"]
    suite = manifest["suite"]
    modes: tuple[str, ...] = tuple(config["modes"])
    repetitions: int = int(config["repetitions"])
    denominator: int = int(suite["denominator"])
    pinned: tuple[str, ...] = tuple(suite["instance_ids"])
    bar = EvidenceBar(**config["evidence_bar"])
    iteration_profile = config.get("iteration_profile")
    # One rule, read once. A manifest without an `iteration_profile` key reads
    # as "measured" rather than as a crash.
    label = not_evidence_label(
        process_runner=config.get("process_runner"),
        iteration_profile=iteration_profile,
    )

    dispositions = _dispositions(grader_cells)
    graded_cells = {(str(c["mode"]), int(c["repetition"])) for c in grader_cells if c.get("graded")}
    scored = _scored_attempts(episodes)

    cells: list[Cell] = []
    for mode, repetition in itertools.product(modes, range(1, repetitions + 1)):
        attempted = {
            instance
            for (m, r, instance) in scored
            if m == mode and r == repetition
        }
        cell_dispositions = dispositions.get((mode, repetition), {})
        resolved = sum(
            1 for instance in pinned if cell_dispositions.get(instance) == RESOLVED
        )
        graded = sum(1 for instance in pinned if instance in cell_dispositions)
        complete = (
            (mode, repetition) in graded_cells
            and len(attempted & set(pinned)) == denominator
            and graded == denominator
        )
        costs = [
            scored[(mode, repetition, instance)]["cost"]
            for instance in attempted
            if (mode, repetition, instance) in scored
        ]
        cells.append(
            Cell(
                mode=mode,
                repetition=repetition,
                resolved=resolved,
                attempted=len(attempted),
                graded=graded,
                denominator=denominator,
                resolved_rate=(100.0 * resolved / denominator) if complete else None,
                measured_cost_usd=None if any(c is None for c in costs) else sum(costs),  # type: ignore[arg-type]
                inference_seconds=sum(
                    scored[(mode, repetition, instance)]["seconds"]
                    for instance in attempted
                    if (mode, repetition, instance) in scored
                ),
                complete=complete,
                not_evidence=label,
            )
        )

    all_complete = bool(cells) and all(cell.complete for cell in cells)
    comparisons = _comparisons(
        modes=modes,
        repetitions=repetitions,
        pinned=pinned,
        dispositions=dispositions,
        bar=bar,
        analysis_seed=int(config["analysis_seed"]),
        eligible=all_complete and repetitions >= bar.min_sweeps and label is None,
    )

    if label is not None:
        conclusion = label
    elif not all_complete:
        conclusion = CONCLUSION_INCOMPLETE
    elif any(c["credible"] for c in comparisons):
        conclusion = CONCLUSION_CREDIBLE
    else:
        conclusion = CONCLUSION_NOT_DISTINGUISHED

    totals = _totals(episodes)
    summary = {
        "schema": SUMMARY_SCHEMA,
        "eval_run_id": config["eval_run_id"],
        "model": config["model"],
        "process_runner": config.get("process_runner"),
        "iteration_profile": iteration_profile,
        "not_evidence": label,
        # No `dataset_pack` member: which pack ran is recorded once, in the
        # manifest's `config`. `eval_run_id` already ties a summary to exactly
        # one manifest.
        "suite": {key: suite[key] for key in suite_keys if key in suite},
        "cells": [cell.to_json() for cell in cells],
        "comparisons": comparisons,
        "evidence_bar": dataclasses.asdict(bar),
        "totals": totals,
        "complete": all_complete,
        "conclusion": conclusion,
        "conclusion_note": _conclusion_note(
            conclusion,
            cells,
            repetitions,
            bar,
            iteration_profile=iteration_profile,
            selected=len(config.get("instance_ids") or ()) or denominator,
            denominator=denominator,
            episode_timeout_seconds=config.get("episode_timeout_seconds"),
        ),
    }
    # Added only when the manifest records an axis, so a summary derived from
    # a manifest without one keeps its bytes. The axis names the interpretation
    # of the ``modes`` field, so topology comparisons stay legible when copied
    # away from their manifest.
    if "axis" in config:
        summary["axis"] = config["axis"]
        summary["graph_search_mode"] = config.get("graph_search_mode")
    return summary


def _scored_attempts(episodes: Iterable[Mapping[str, Any]]) -> dict[tuple[str, int, str], dict[str, Any]]:
    """The one attempt that counts for each ``(mode, repetition, instance)``.

    Every attempt stays in ``episodes.jsonl``; this picks the highest-numbered
    one, which is the retry when a confirmed infrastructure failure was retried
    and the sole attempt otherwise. Nothing is dropped from the record — only
    from the arithmetic, and only in favour of the later attempt.
    """
    chosen: dict[tuple[str, int, str], dict[str, Any]] = {}
    for episode in episodes:
        key = (str(episode["mode"]), int(episode["repetition"]), str(episode["instance_id"]))
        attempt = int(episode.get("attempt", 1))
        previous = chosen.get(key)
        if previous is not None and previous["attempt"] >= attempt:
            continue
        usage = episode.get("usage")
        chosen[key] = {
            "attempt": attempt,
            "cost": None if not usage else usage.get("estimated_cost_usd"),
            "seconds": float(episode.get("inference_seconds", 0.0)),
        }
    return chosen


def _dispositions(cells: Iterable[Mapping[str, Any]]) -> dict[tuple[str, int], dict[str, str]]:
    """Per-cell official dispositions, keyed by instance."""
    from .report import CellReport

    out: dict[tuple[str, int], dict[str, str]] = {}
    for raw in cells:
        report = CellReport.from_json(raw)
        if not report.graded:
            continue
        mapping: dict[str, str] = {}
        for instance in report.resolved_ids | report.unresolved_ids | report.empty_patch_ids | report.error_ids:
            mapping[instance] = report.disposition(instance)
        out[(report.mode, report.repetition)] = mapping
    return out


def _comparisons(
    *,
    modes: Sequence[str],
    repetitions: int,
    pinned: Sequence[str],
    dispositions: Mapping[tuple[str, int], Mapping[str, str]],
    bar: EvidenceBar,
    analysis_seed: int,
    eligible: bool,
) -> list[dict[str, Any]]:
    """Every mode pair, in canonical order, reported whether or not it is nice.

    Every pair is reported so callers cannot select only a favorable one.
    """
    from .statistics import paired_cluster_bootstrap

    out: list[dict[str, Any]] = []
    for left, right in itertools.combinations(modes, 2):
        flags_left = _flags(left, repetitions, pinned, dispositions)
        flags_right = _flags(right, repetitions, pinned, dispositions)
        per_repetition = [
            100.0 * _rate(flags_left, pinned, rep) - 100.0 * _rate(flags_right, pinned, rep)
            for rep in range(1, repetitions + 1)
        ]
        interval = paired_cluster_bootstrap(
            cluster_ids=pinned,
            a=flags_left,
            b=flags_right,
            seed=analysis_seed,
            resamples=bar.bootstrap_resamples,
            confidence=bar.confidence,
        )
        signs = {_sign(d) for d in per_repetition if d != 0.0}
        consistent = len(signs) <= 1 and all(d != 0.0 for d in per_repetition)
        credible = bool(
            eligible
            and repetitions >= bar.min_sweeps
            and abs(interval.difference_points) >= bar.min_absolute_difference_points
            and (consistent or not bar.require_consistent_direction)
            and interval.excludes_zero
        )
        out.append(
            {
                "pair": f"{left}-{right}",
                "per_repetition_difference_points": [round(d, 6) for d in per_repetition],
                "direction_consistent": consistent,
                "bootstrap": interval.to_json(),
                "meets_minimum_sweeps": repetitions >= bar.min_sweeps,
                "meets_minimum_difference": abs(interval.difference_points)
                >= bar.min_absolute_difference_points,
                "eligible": eligible,
                "credible": credible,
            }
        )
    return out


def _flags(
    mode: str,
    repetitions: int,
    pinned: Sequence[str],
    dispositions: Mapping[tuple[str, int], Mapping[str, str]],
) -> dict[str, list[bool]]:
    """Per-instance resolution flags for one mode, one entry per repetition."""
    out: dict[str, list[bool]] = {instance: [] for instance in pinned}
    for rep in range(1, repetitions + 1):
        cell = dispositions.get((mode, rep), {})
        for instance in pinned:
            out[instance].append(cell.get(instance) == RESOLVED)
    return out


def _rate(flags: Mapping[str, Sequence[bool]], pinned: Sequence[str], repetition: int) -> float:
    """Resolved fraction of one repetition, over the fixed denominator."""
    if not pinned:
        return 0.0
    index = repetition - 1
    resolved = sum(
        1
        for instance in pinned
        if index < len(flags.get(instance, ())) and flags[instance][index]
    )
    return resolved / len(pinned)


def _sign(value: float) -> int:
    """The sign of ``value`` as ``-1``, ``0`` or ``1``."""
    return (value > 0) - (value < 0)


def _totals(episodes: Sequence[Mapping[str, Any]]) -> dict[str, Any]:
    """Token, cost and time totals across every retained attempt.

    ``measured_cost_usd`` is ``None`` when any attempt reported an unknown
    cost, because a partial sum presented as a total would understate spend.
    """
    tokens = {"input": 0, "output": 0, "cache_read": 0, "cache_creation": 0}
    cost = 0.0
    unknown = 0
    seconds = 0.0
    for episode in episodes:
        seconds += float(episode.get("inference_seconds", 0.0))
        usage = episode.get("usage")
        if not usage:
            unknown += 1
            continue
        tokens["input"] += int(usage.get("input_tokens", 0))
        tokens["output"] += int(usage.get("output_tokens", 0))
        tokens["cache_read"] += int(usage.get("cache_read_tokens", 0))
        tokens["cache_creation"] += int(usage.get("cache_creation_tokens", 0))
        if usage.get("estimated_cost_usd") is None:
            unknown += 1
        else:
            cost += float(usage["estimated_cost_usd"])
    return {
        "episodes": len(episodes),
        "tokens": tokens,
        "measured_cost_usd": None if unknown else round(cost, 6),
        "priced_cost_usd": round(cost, 6),
        "unknown_cost_episodes": unknown,
        "inference_seconds": round(seconds, 3),
    }


def _conclusion_note(
    conclusion: str,
    cells: Sequence[Cell],
    repetitions: int,
    bar: EvidenceBar,
    *,
    iteration_profile: str | None = None,
    selected: int = 0,
    denominator: int = 0,
    episode_timeout_seconds: int | None = None,
) -> str:
    """One plain sentence explaining the conclusion to a human reader."""
    if conclusion == CONCLUSION_ITERATION:
        return iteration_note(
            name=str(iteration_profile),
            selected=selected,
            denominator=denominator,
            repetitions=repetitions,
            episode_timeout_seconds=episode_timeout_seconds,
        )
    if conclusion == CONCLUSION_PLUMBING:
        # A replayed iteration run gets both facts, in the order that matters:
        # the fixtures first, because knowing the profile would not help a
        # reader who thought the numbers had been observed.
        profile = (
            ""
            if not iteration_profile
            else f" It was configured from iteration profile {iteration_profile!r}, which is"
            " itself not evidence."
        )
        return (
            "Episodes were replayed from a recorded script rather than executed, so no "
            "agent ran, no container was graded and no money was spent. This directory "
            "demonstrates that the runner works end to end; it is not a benchmark result."
            + profile
        )
    if conclusion == CONCLUSION_INCOMPLETE:
        incomplete = [f"{c.mode}/r{c.repetition}" for c in cells if not c.complete]
        return (
            "No primary score: "
            + ", ".join(incomplete)
            + f" did not attempt and grade all {cells[0].denominator if cells else 0} pinned "
            "instances, and the denominator is fixed."
        )
    if conclusion == CONCLUSION_NOT_DISTINGUISHED:
        return (
            f"Every mode pair fell short of the evidence bar "
            f"({bar.min_sweeps} sweeps, {bar.min_absolute_difference_points} points, "
            "consistent direction, interval excluding zero)."
        )
    return (
        f"At least one mode pair cleared the evidence bar over {repetitions} sweeps. "
        "The claim is about this agent commit, model, budget and pinned suite only."
    )


def render(summary: Mapping[str, Any]) -> str:
    """Serialize a summary deterministically, as ``recompute`` compares bytes."""
    return json.dumps(summary, indent=2, sort_keys=True, ensure_ascii=False) + "\n"
