"""Pinned official SWE-bench grading for the Verified pack.

The prediction interface is shared with the Multilingual pack, but the pinned
official evaluator writes its current reports under
``logs/evaluation/<run>/results.json``. That filesystem convention is
an evaluator/adaptor fact, not a reason to change the neutral core.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Mapping, Sequence

from ...core.process import ProcessRunner
from ...core.report import CellReport, ungraded
from ..swebench_multilingual import grader as shared

GRADER_MODULE = shared.GRADER_MODULE
MERGED_PREDICTIONS = shared.MERGED_PREDICTIONS
grader_argv = shared.grader_argv
grader_run_id = shared.grader_run_id
model_name = shared.model_name
parse_report = shared.parse_report
prediction = shared.prediction
write_predictions = shared.write_predictions


class Grader:
    """Invoke the pinned official evaluator and read its official summary."""

    def __init__(
        self,
        runner: ProcessRunner,
        *,
        dataset: str,
        max_workers: int,
        timeout: int,
        python: str = "python",
    ) -> None:
        self._runner = runner
        self._dataset = dataset
        self._max_workers = max_workers
        self._timeout = timeout
        self._python = python
        self._merged: list[Mapping[str, str]] = []

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
        """Grade one cell with no locally inferred correctness verdict."""
        run_id = grader_run_id(eval_run_id, mode, repetition)
        work_dir = run_dir / "grader"
        work_dir.mkdir(parents=True, exist_ok=True)
        cell_predictions = [
            prediction(
                instance_id,
                patches[instance_id],
                eval_run_id=eval_run_id,
                mode=mode,
                repetition=repetition,
            )
            for instance_id in instance_ids
        ]
        self._merged.extend(cell_predictions)
        predictions_path = run_dir / "predictions" / mode / f"r{repetition}.jsonl"
        write_predictions(predictions_path, cell_predictions)
        finished = self._runner.run(
            grader_argv(
                dataset=self._dataset,
                # The grader runs from `work_dir`, so a relative path would not resolve.
                predictions_path=predictions_path.resolve(),
                instance_ids=instance_ids,
                max_workers=self._max_workers,
                timeout=self._timeout,
                run_id=run_id,
                python=self._python,
            ),
            timeout=self._timeout * max(1, len(instance_ids)),
            cwd=work_dir,
        )
        if not finished.ok:
            return ungraded(
                mode=mode,
                repetition=repetition,
                run_id=run_id,
                exit_code=finished.returncode,
                note=f"grader did not complete: {finished.stderr.strip()[:400]}",
            )
        report = _locate_report(work_dir, model_name(eval_run_id, mode, repetition), run_id, finished.stdout)
        if report is None:
            return ungraded(
                mode=mode,
                repetition=repetition,
                run_id=run_id,
                exit_code=finished.returncode,
                note="grader exited 0 but wrote no report file",
            )
        return parse_report(
            report,
            mode=mode,
            repetition=repetition,
            run_id=run_id,
            exit_code=finished.returncode,
        )

    def write_merged_predictions(self, run_dir: Path) -> None:
        """Write the complete run's official-format prediction JSONL."""
        write_predictions(run_dir / MERGED_PREDICTIONS, self._merged)


def _locate_report(work_dir: Path, model: str, run_id: str, stdout: str) -> dict[str, object] | None:
    """Read the pinned evaluator's results file, then legacy locations."""
    current = work_dir / "logs" / "evaluation" / run_id / "results.json"
    candidates = [current, *sorted(work_dir.glob(f"*{run_id}*.json"))]
    for candidate in candidates:
        if candidate.is_file():
            try:
                loaded = json.loads(candidate.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                continue
            if isinstance(loaded, dict):
                return loaded
    return shared._locate_report(work_dir, model, run_id, stdout)


__all__ = [
    "GRADER_MODULE",
    "MERGED_PREDICTIONS",
    "Grader",
    "grader_argv",
    "grader_run_id",
    "model_name",
    "parse_report",
    "prediction",
    "write_predictions",
]
