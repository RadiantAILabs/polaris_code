"""The official SWE-bench grader, invoked per matrix cell.

Correctness is not this adapter's to decide. Every disposition in a run
directory comes from ``python -m swebench.harness.run_evaluation`` at the
pinned grader revision, which applies the official test patch and the
fail-to-pass / pass-to-pass checks inside the instance's own image.

One cell at a time, and never one big grading pass, because SWE-bench caches by
``(run_id, instance_id)``. A single predictions file holding the same instance
three times — once per mode — would either collide in the cache or silently
grade one mode's patch under another's name. So each ``(mode, repetition)``
cell gets its own predictions file, its own ``--run_id`` and its own report,
and :func:`grader_run_id` is the function that keeps those apart.

The prediction JSONL shape, the report filename and the ``run_id`` convention
are all SWE-bench's, which is why this whole module is pack-local. What the
core gets back is a :class:`~evals.core.report.CellReport` in the shared
disposition vocabulary.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable, Mapping, Sequence

from ...core.process import ProcessRunner
from ...core.report import CellReport, ungraded

#: The grader module the official harness exposes.
GRADER_MODULE = "swebench.harness.run_evaluation"

#: The merged prediction artifact this pack writes into a run directory.
MERGED_PREDICTIONS = "predictions.jsonl"


def grader_run_id(eval_run_id: str, mode: str, repetition: int) -> str:
    """A grader run id unique to one matrix cell.

    SWE-bench caches by ``(run_id, instance_id)``. Sharing a run id across
    modes would serve one mode's cached verdict for another mode's patch, so
    the mode and repetition are part of the id rather than of a subdirectory.
    """
    return f"{eval_run_id}-{mode}-r{repetition}"


def model_name(eval_run_id: str, mode: str, repetition: int) -> str:
    """The ``model_name_or_path`` recorded in a prediction.

    The official harness names its report file after this, so it carries the
    cell as well, which keeps two cells' reports apart on disk even when a
    caller reuses one output directory.
    """
    return f"polaris-code+{mode}+r{repetition}"


def prediction(instance_id: str, patch: str, *, eval_run_id: str, mode: str, repetition: int) -> dict[str, str]:
    """One line of the official prediction JSONL shape."""
    return {
        "instance_id": instance_id,
        "model_name_or_path": model_name(eval_run_id, mode, repetition),
        "model_patch": patch,
    }


def write_predictions(path: Path, predictions: Iterable[Mapping[str, str]]) -> None:
    """Write a predictions file in the official one-object-per-line shape."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        for record in predictions:
            handle.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")


def grader_argv(
    *,
    dataset: str,
    predictions_path: Path,
    instance_ids: Sequence[str],
    max_workers: int,
    timeout: int,
    run_id: str,
    python: str = "python",
) -> tuple[str, ...]:
    """The official grader command for one cell, as argv.

    Passes the instance list explicitly so a cell grades the instances it
    actually attempted and nothing else.
    """
    return (
        python,
        "-m",
        GRADER_MODULE,
        "--dataset_name",
        dataset,
        "--predictions_path",
        str(predictions_path),
        "--instance_ids",
        *instance_ids,
        "--max_workers",
        str(max_workers),
        "--timeout",
        str(timeout),
        "--run_id",
        run_id,
    )


def parse_report(raw: Mapping[str, object], *, mode: str, repetition: int, run_id: str, exit_code: int) -> CellReport:
    """Reduce one official report object to a :class:`CellReport`."""
    return CellReport(
        mode=mode,
        repetition=repetition,
        run_id=run_id,
        resolved_ids=frozenset(str(x) for x in raw.get("resolved_ids", ())),
        unresolved_ids=frozenset(str(x) for x in raw.get("unresolved_ids", ())),
        empty_patch_ids=frozenset(str(x) for x in raw.get("empty_patch_instances_ids", raw.get("empty_patch_ids", ()))),
        error_ids=frozenset(str(x) for x in raw.get("error_ids", ())),
        graded=True,
        exit_code=exit_code,
    )


class Grader:
    """Invokes the official grader for one cell and reads back its report.

    Also owns this dataset's prediction artifacts: one file per cell under
    ``predictions/<mode>/r<repetition>.jsonl`` because the grader needs one,
    plus the merged ``predictions.jsonl`` that a reader of a finished run
    expects. The merged file is written once, after every cell, in the order
    the cells were graded.
    """

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
        """Grade one cell, returning an ungraded report rather than raising.

        A grader that will not run leaves the cell *incomplete*, which the
        collation then refuses to score. That is the honest outcome: an
        ungradable cell has no resolved rate, and inventing one by counting its
        instances as unresolved would report a measurement that was never made.
        """
        run_id = grader_run_id(eval_run_id, mode, repetition)
        work_dir = run_dir / "grader"
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

        argv = grader_argv(
            dataset=self._dataset,
            # The grader runs from `work_dir`, so a relative path would not resolve.
            predictions_path=predictions_path.resolve(),
            instance_ids=instance_ids,
            max_workers=self._max_workers,
            timeout=self._timeout,
            run_id=run_id,
            python=self._python,
        )
        finished = self._runner.run(argv, timeout=self._timeout * max(1, len(instance_ids)), cwd=work_dir)
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
        return parse_report(report, mode=mode, repetition=repetition, run_id=run_id, exit_code=finished.returncode)

    def write_merged_predictions(self, run_dir: Path) -> None:
        """Write the whole-run ``predictions.jsonl``."""
        write_predictions(run_dir / MERGED_PREDICTIONS, self._merged)


def _locate_report(work_dir: Path, model: str, run_id: str, stdout: str) -> dict[str, object] | None:
    """Find the report the official harness wrote for one cell.

    The harness names it ``<model_name_or_path>.<run_id>.json`` in its working
    directory. Both the exact name and a glob are tried, and the grader's own
    stdout is searched last, because the filename has moved between grader
    revisions and a run must not lose its verdicts to a rename.
    """
    exact = work_dir / f"{model}.{run_id}.json"
    candidates = [exact, *sorted(work_dir.glob(f"*{run_id}*.json"))]
    for candidate in candidates:
        if candidate.is_file():
            try:
                loaded = json.loads(candidate.read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                continue
            if isinstance(loaded, dict):
                return loaded
    for line in stdout.splitlines():
        token = line.strip()
        if token.endswith(".json") and Path(token).is_file():
            try:
                loaded = json.loads(Path(token).read_text(encoding="utf-8"))
            except json.JSONDecodeError:
                continue
            if isinstance(loaded, dict):
                return loaded
    return None
