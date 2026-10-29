"""The deterministic paired task-cluster bootstrap.

The evidence bar requires a two-sided 95% interval that excludes zero,
computed by resampling *task clusters* rather than individual episodes.
Clustering matters: the same instance appears once per mode,
so episodes are not independent, and resampling them individually would make
every interval far too narrow.

Determinism matters just as much, because ``recompute`` must byte-reproduce
``summary.json`` from the same inputs. The procedure is fixed here and recorded
in the summary:

1. Order the clusters by instance id, ascending.
2. Seed :class:`random.Random` with the run's recorded ``analysis_seed``.
3. For each of ``resamples`` iterations draw ``len(clusters)`` cluster indices
   with replacement via ``randrange``, in order.
4. Difference the two modes' mean per-cluster resolution over the resample.
5. Report the empirical percentile interval at the recorded confidence.

Nothing here consults the clock, the platform or a hash seed, so two hosts with
the same Python major version produce identical intervals.
"""

from __future__ import annotations

import dataclasses
import random
from typing import Mapping, Sequence


@dataclasses.dataclass(frozen=True)
class Interval:
    """A two-sided percentile interval for a paired difference."""

    #: Point estimate of ``a - b``, in percentage points.
    difference_points: float
    low_points: float
    high_points: float
    confidence: float
    resamples: int
    seed: int
    #: Clusters the interval was computed over.
    clusters: int

    @property
    def excludes_zero(self) -> bool:
        """Whether the whole interval lies on one side of zero."""
        return (self.low_points > 0.0) or (self.high_points < 0.0)

    def to_json(self) -> dict[str, object]:
        """The summary projection."""
        return {
            "difference_points": round(self.difference_points, 6),
            "low_points": round(self.low_points, 6),
            "high_points": round(self.high_points, 6),
            "confidence": self.confidence,
            "resamples": self.resamples,
            "seed": self.seed,
            "clusters": self.clusters,
            "excludes_zero": self.excludes_zero,
        }


def paired_cluster_bootstrap(
    *,
    cluster_ids: Sequence[str],
    a: Mapping[str, Sequence[bool]],
    b: Mapping[str, Sequence[bool]],
    seed: int,
    resamples: int,
    confidence: float,
) -> Interval:
    """Bootstrap the paired difference in resolved rate between two modes.

    Args:
        cluster_ids: every task cluster, in ascending instance-id order. This
            is the *fixed* denominator, so a cluster with no attempt in either
            mode still counts — as a zero — and cannot be dropped to improve a
            rate.
        a: per-cluster resolution flags for the first mode, one per repetition.
        b: the same for the second mode.
        seed: the run's recorded analysis seed.
        resamples: iterations; the evidence bar fixes 10,000.
        confidence: two-sided level, e.g. ``0.95``.

    Returns:
        The point difference ``rate(a) - rate(b)`` in percentage points and its
        interval.

    Raises:
        ValueError: if ``resamples`` is not positive or ``confidence`` is not
            strictly between 0 and 1.
    """
    if resamples <= 0:
        raise ValueError(f"resamples must be positive, got {resamples}")
    if not 0.0 < confidence < 1.0:
        raise ValueError(f"confidence must lie in (0, 1), got {confidence}")

    ordered = sorted(cluster_ids)
    means_a = [_cluster_mean(a.get(cluster, ())) for cluster in ordered]
    means_b = [_cluster_mean(b.get(cluster, ())) for cluster in ordered]
    count = len(ordered)
    point = 100.0 * (_mean(means_a) - _mean(means_b))

    if count == 0:
        return Interval(0.0, 0.0, 0.0, confidence, resamples, seed, 0)

    rng = random.Random(seed)
    differences: list[float] = []
    for _ in range(resamples):
        indices = [rng.randrange(count) for _ in range(count)]
        resampled_a = sum(means_a[i] for i in indices) / count
        resampled_b = sum(means_b[i] for i in indices) / count
        differences.append(100.0 * (resampled_a - resampled_b))
    differences.sort()

    tail = (1.0 - confidence) / 2.0
    low = _percentile(differences, tail)
    high = _percentile(differences, 1.0 - tail)
    return Interval(point, low, high, confidence, resamples, seed, count)


def _cluster_mean(flags: Sequence[bool]) -> float:
    """Mean resolution within one cluster, across its repetitions.

    A cluster with no attempt is ``0.0``: an instance the run never reached is
    unresolved, which is what a fixed denominator means.
    """
    if not flags:
        return 0.0
    return sum(1.0 for flag in flags if flag) / len(flags)


def _mean(values: Sequence[float]) -> float:
    """Arithmetic mean, defined as zero for an empty sequence."""
    return sum(values) / len(values) if values else 0.0


def _percentile(sorted_values: Sequence[float], q: float) -> float:
    """The ``q`` quantile of an already-sorted sequence, by nearest rank.

    Nearest rank rather than interpolation so the result is a value the
    bootstrap actually produced, and so two implementations of the same
    procedure cannot disagree in the last decimal.
    """
    if not sorted_values:
        return 0.0
    index = int(round(q * (len(sorted_values) - 1)))
    return sorted_values[max(0, min(len(sorted_values) - 1, index))]
