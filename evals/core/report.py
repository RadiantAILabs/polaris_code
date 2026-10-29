"""One cell's dispositions, in the only vocabulary the evidence bar reads.

Correctness is never the harness's to decide — a pack's grader adapter owns
that. What *is* shared is the shape the answer arrives in, because the evidence
bar, the collation and the fixed denominator all depend on being able to ask
one question of one cell: what did the official grader say about this instance?

Five answers exist and no sixth is allowed. ``not-graded`` is the honest answer
for a cell whose grader would not run; :mod:`evals.core.collate` then refuses
to score the cell rather than counting its instances as unresolved, because
inventing a verdict reports a measurement nobody made.
"""

from __future__ import annotations

import dataclasses
from typing import Mapping

#: Dispositions a graded instance may carry in an episode record.
RESOLVED = "resolved"
UNRESOLVED = "unresolved"
EMPTY_PATCH = "empty-patch"
ERROR = "grader-error"
NOT_GRADED = "not-graded"


@dataclasses.dataclass(frozen=True)
class CellReport:
    """One cell's official grader report, reduced to what collation needs."""

    mode: str
    repetition: int
    run_id: str
    resolved_ids: frozenset[str]
    unresolved_ids: frozenset[str]
    empty_patch_ids: frozenset[str]
    error_ids: frozenset[str]
    #: Whether the grader itself ran to completion for this cell.
    graded: bool
    #: The grader's own exit code, or ``None`` when it was never invoked.
    exit_code: int | None
    #: A short, secret-free note when the grader could not be run.
    note: str | None = None

    def disposition(self, instance_id: str) -> str:
        """The official disposition of one instance in this cell."""
        if not self.graded:
            return NOT_GRADED
        if instance_id in self.resolved_ids:
            return RESOLVED
        if instance_id in self.empty_patch_ids:
            return EMPTY_PATCH
        if instance_id in self.error_ids:
            return ERROR
        if instance_id in self.unresolved_ids:
            return UNRESOLVED
        # Graded but unmentioned. Counted as unresolved, never dropped: the
        # denominator is fixed and a missing verdict must not shrink it.
        return UNRESOLVED

    def to_json(self) -> dict[str, object]:
        """The ``grader/results.json`` projection for this cell."""
        return {
            "mode": self.mode,
            "repetition": self.repetition,
            "run_id": self.run_id,
            "graded": self.graded,
            "exit_code": self.exit_code,
            "note": self.note,
            "resolved_ids": sorted(self.resolved_ids),
            "unresolved_ids": sorted(self.unresolved_ids),
            "empty_patch_ids": sorted(self.empty_patch_ids),
            "error_ids": sorted(self.error_ids),
        }

    @classmethod
    def from_json(cls, raw: Mapping[str, object]) -> "CellReport":
        """Read a cell back from ``grader/results.json``."""
        return cls(
            mode=str(raw["mode"]),
            repetition=int(raw["repetition"]),
            run_id=str(raw["run_id"]),
            resolved_ids=frozenset(str(x) for x in raw.get("resolved_ids", ())),
            unresolved_ids=frozenset(str(x) for x in raw.get("unresolved_ids", ())),
            empty_patch_ids=frozenset(str(x) for x in raw.get("empty_patch_ids", ())),
            error_ids=frozenset(str(x) for x in raw.get("error_ids", ())),
            graded=bool(raw.get("graded", False)),
            exit_code=None if raw.get("exit_code") is None else int(raw["exit_code"]),  # type: ignore[arg-type]
            note=None if raw.get("note") is None else str(raw["note"]),
        )


def ungraded(
    *, mode: str, repetition: int, run_id: str, exit_code: int | None, note: str
) -> CellReport:
    """The report of a cell whose grader did not produce verdicts.

    Shared so that every pack's adapter reports "I could not grade this" the
    same way, and so that no adapter is tempted to return an empty *graded*
    report, which would silently score the whole cell zero.
    """
    return CellReport(
        mode=mode,
        repetition=repetition,
        run_id=run_id,
        resolved_ids=frozenset(),
        unresolved_ids=frozenset(),
        empty_patch_ids=frozenset(),
        error_ids=frozenset(),
        graded=False,
        exit_code=exit_code,
        note=note,
    )
