"""The sweep: schedule episodes, grade cells, collate, and refuse to lie.

The order of operations is fixed and enforced here:

1. **Refuse an unaffordable run before starting it.** The projection uses the
   configured per-episode planning rate; over the ceiling, nothing runs.
2. **Refuse a non-empty output directory.** A run directory is immutable, so
   reusing one would overwrite evidence.
3. **Randomise mode order per ``(instance, repetition)``** from the recorded
   order seed, so a systematic drift over the sweep cannot be mistaken for a
   mode effect.
4. **One fresh environment per episode**, appended to ``episodes.jsonl`` as it
   finishes. The file is append-only, so an interrupted sweep still holds every
   episode that completed.
5. **Stop scheduling at the ceiling**, let started episodes finish, mark the
   run incomplete, emit no primary score.
6. **Grade per cell**, through the pack's grader adapter, which is responsible
   for keeping two cells' verdicts apart.
7. **Collate**, and let :mod:`evals.core.collate` decide what
   may be claimed.

None of those seven steps names a dataset. What the sweep does *not* own is the
environment each episode runs in and the grader that judges it: both come from
the :class:`~evals.core.pack.DatasetPack` handed in, which is the whole reason
a second dataset is a new pack rather than an edit here.

Episodes run sequentially unless ``episode_workers`` is above one, in which
case that many episodes run at once. The cost guard is still asked before each
episode starts, so at most ``episode_workers - 1`` episodes can start after
spend reaches the ceiling; they finish, as step 5 says. ``episodes.jsonl`` is
then in finishing order, and the returned records are put back in schedule
order. ``max_workers`` is recorded and handed to the pack's grader, which
parallelises grading itself.
"""

from __future__ import annotations

import dataclasses
import json
import random
import threading
from concurrent.futures import ThreadPoolExecutor
import time
from pathlib import Path
from typing import Any, Callable, Sequence

from .budget import CostGuard
from .collate import DEFAULT_SUMMARY_SUITE_KEYS
from .collate import render as render_summary
from .collate import summarise
from .config import (
    AXIS_COMPOSITION,
    COMPOSITION_POLICY_PLANNING_COST_PER_EPISODE_USD,
    EPISODE_SCHEMA,
    INFRASTRUCTURE_RETRIES,
    RunConfig,
)
from .episode import EpisodeOutcome
from .manifest import build as build_manifest
from .manifest import finalise, render as render_manifest, repository_state, sha256_file
from .pack import DatasetPack, Instance, Suite
from .plan_history import append_graded_episodes
from .process import ProcessRunner


class RunDirectoryExists(RuntimeError):
    """``--output`` already holds a run. Run directories are immutable."""


class DirtyTree(RuntimeError):
    """A measured run was started from a checkout with uncommitted changes."""


class PackMismatch(RuntimeError):
    """A run directory was recomputed with a pack that did not produce it."""


@dataclasses.dataclass
class SweepResult:
    """What one ``run`` produced, for the CLI to report."""

    run_dir: Path
    manifest: dict[str, Any]
    summary: dict[str, Any]
    episodes: list[dict[str, Any]]


def plan_order(
    *,
    instances: Sequence[Instance],
    modes: Sequence[str],
    repetitions: int,
    order_seed: int,
) -> list[tuple[Instance, str, int]]:
    """The full episode schedule, with mode order shuffled per cell.

    The shuffle is seeded per ``(instance, repetition)`` from ``order_seed`` so
    the schedule is a pure function of the recorded configuration: two runs of
    the same manifest attempt the same episodes in the same order.
    """
    schedule: list[tuple[Instance, str, int]] = []
    for repetition in range(1, repetitions + 1):
        for instance in instances:
            ordered = list(modes)
            random.Random(f"{order_seed}:{instance.instance_id}:{repetition}").shuffle(ordered)
            schedule.extend((instance, mode, repetition) for mode in ordered)
    return schedule


def run_sweep(
    *,
    config: RunConfig,
    pack: DatasetPack,
    suite: Suite,
    output: Path,
    process_runner: ProcessRunner,
    command_line: Sequence[str],
    repositories: Sequence[Path] = (),
    api_key: str | None = None,
    options: Any = None,
    require_clean_tree: bool = True,
    plan_history: Path | None = None,
    now: Callable[[], str] = lambda: time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    progress: Callable[[str], None] = lambda _message: None,
    episode_workers: int = 1,
) -> SweepResult:
    """Run one sweep into a fresh, immutable directory.

    Args:
        pack: the dataset under evaluation. Supplies the environment every
            episode runs in, the grader that judges it, the extra artifacts the
            manifest digests and the suite keys the summary repeats.
        suite: ``pack.load_suite()``, passed in rather than re-read so a test
            can substitute a small suite without touching the pinned file.
        options: the parsed command line, handed to the pack's providers for
            the flags only they understand. ``None`` for a programmatic sweep,
            which is what the tests do.
        episode_workers: how many episodes run at once. One (the default)
            runs them one after another.

    Raises:
        RunDirectoryExists: if ``output`` exists and is not empty.
        BudgetExceeded: if the projection exceeds the configured ceiling.
        DirtyTree: if a measured run was started from a dirty checkout.
        SuiteError: if ``config.instance_ids`` names an unpinned instance.
    """
    if output.exists() and any(output.iterdir()):
        raise RunDirectoryExists(
            f"{output} already holds a run; a run directory is immutable, choose a new --output"
        )

    instances = suite.select(config.instance_ids)
    episodes_planned = len(instances) * len(config.modes) * config.repetitions
    guard = CostGuard(ceiling_usd=config.api_cost_ceiling_usd)
    if config.axis == AXIS_COMPOSITION:
        # Each mode is a composition-policy identity, and `model` can spend
        # more per episode than `deterministic` (the chooser's own bounded
        # worst case, on top of the ordinary planning cost) — one flat rate
        # over every episode would under-count what a `model` cell can spend.
        per_mode_episodes = {
            mode: len(instances) * config.repetitions for mode in config.modes
        }
        projected = guard.check_projection_by_mode(
            per_mode_episodes, COMPOSITION_POLICY_PLANNING_COST_PER_EPISODE_USD
        )
    else:
        projected = guard.check_projection(episodes_planned)

    repo_states = [repository_state(path) for path in repositories]
    if require_clean_tree:
        dirty = [r["path"] for r in repo_states if r["clean"] is False]
        if dirty:
            raise DirtyTree(
                "refusing to start a measured run from a dirty checkout: "
                + ", ".join(dirty)
                + "; commit or stash, or pass --allow-dirty for a plumbing run"
            )

    binary = Path(config.polaris_code_binary)
    binary_sha = sha256_file(binary) if binary.is_file() else None
    binary_bytes = binary.stat().st_size if binary.is_file() else None

    output.mkdir(parents=True, exist_ok=True)
    (output / "grader").mkdir(exist_ok=True)
    (output / "logs").mkdir(exist_ok=True)

    manifest = build_manifest(
        config=config,
        pack=pack,
        suite=suite,
        selected_instance_ids=[i.instance_id for i in instances],
        binary_sha256=binary_sha,
        binary_bytes=binary_bytes,
        repositories=repo_states,
        command_line=command_line,
        created_at=now(),
        projected_cost_usd=projected,
        episodes_planned=episodes_planned,
    )
    manifest["episode_workers"] = episode_workers
    (output / "manifest.json").write_text(render_manifest(manifest), encoding="utf-8")
    (output / "instance_ids.txt").write_text(
        "".join(f"{i}\n" for i in suite.instance_ids), encoding="utf-8"
    )

    environment = pack.environment(
        config=config,
        suite=suite,
        process_runner=process_runner,
        api_key=api_key,
        options=options,
    )

    records: list[dict[str, Any]] = []
    patches: dict[tuple[str, int, str], str] = {}
    episode_records: dict[tuple[str, int, str], dict[str, Any]] = {}
    episodes_path = output / "episodes.jsonl"
    schedule = plan_order(
        instances=instances,
        modes=config.modes,
        repetitions=config.repetitions,
        order_seed=config.order_seed,
    )
    lock = threading.Lock()
    halted = threading.Event()
    ordered: dict[int, list[dict[str, Any]]] = {}

    def run_one(index: int, instance: Instance, mode: str, repetition: int) -> None:
        with lock:
            if halted.is_set() or not guard.may_schedule():
                if not halted.is_set():
                    halted.set()
                    progress(f"halting: {guard.halt_reason}")
                return
        for attempt in range(1, INFRASTRUCTURE_RETRIES + 2):
            outcome = environment.run(
                instance=instance,
                mode=mode,
                repetition=repetition,
                attempt=attempt,
                eval_run_id=config.eval_run_id,
                model=config.model,
            )
            record = outcome.to_json(EPISODE_SCHEMA)
            with lock:
                ordered.setdefault(index, []).append(record)
                log.write(json.dumps(record, sort_keys=True, ensure_ascii=False) + "\n")
                log.flush()
                _write_patch(output, outcome)
                guard.record(outcome.usage, billable=outcome.agent_was_launched)
                patches[(mode, repetition, instance.instance_id)] = outcome.patch
                episode_records[(mode, repetition, instance.instance_id)] = record
                progress(
                    f"{mode}/r{repetition} {instance.instance_id}: {outcome.termination}"
                )
            # Only a confirmed infrastructure failure is retried, and only
            # once. A failed turn, a timeout and an empty patch are results.
            if not outcome.is_infrastructure_failure:
                break

    with episodes_path.open("w", encoding="utf-8") as log:
        if episode_workers <= 1:
            for index, (instance, mode, repetition) in enumerate(schedule):
                run_one(index, instance, mode, repetition)
                if halted.is_set():
                    break
        else:
            with ThreadPoolExecutor(max_workers=episode_workers) as pool:
                futures = [
                    pool.submit(run_one, index, instance, mode, repetition)
                    for index, (instance, mode, repetition) in enumerate(schedule)
                ]
                for future in futures:
                    future.result()
    for index in sorted(ordered):
        records.extend(ordered[index])

    grader = pack.grader(
        config=config,
        suite=suite,
        process_runner=process_runner,
        options=options,
    )
    cells: list[dict[str, Any]] = []
    for mode in config.modes:
        for repetition in range(1, config.repetitions + 1):
            attempted = [
                i.instance_id
                for i in instances
                if (mode, repetition, i.instance_id) in patches
            ]
            if not attempted:
                continue
            report = grader.grade(
                eval_run_id=config.eval_run_id,
                mode=mode,
                repetition=repetition,
                patches={
                    instance_id: patches[(mode, repetition, instance_id)]
                    for instance_id in attempted
                },
                instance_ids=attempted,
                run_dir=output,
            )
            cells.append(report.to_json())
            if plan_history is not None:
                append_graded_episodes(
                    plan_history,
                    report=report,
                    episodes={
                        instance_id: episode_records[(mode, repetition, instance_id)]
                        for instance_id in attempted
                    },
                )
            progress(f"graded {mode}/r{repetition}: {len(report.resolved_ids)} resolved")

    grader.write_merged_predictions(output)
    (output / "grader" / "results.json").write_text(
        json.dumps({"cells": cells}, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )

    manifest = finalise(manifest, output, extra_outputs=pack.digested_outputs())
    manifest["budget"] = guard.to_json()
    (output / "manifest.json").write_text(render_manifest(manifest), encoding="utf-8")

    summary = summarise(
        manifest=manifest,
        episodes=records,
        grader_cells=cells,
        suite_keys=pack.summary_suite_keys(),
    )
    (output / "summary.json").write_text(render_summary(summary), encoding="utf-8")
    return SweepResult(run_dir=output, manifest=manifest, summary=summary, episodes=records)


def recompute(
    run_dir: Path,
    *,
    suite_keys: Sequence[str] = DEFAULT_SUMMARY_SUITE_KEYS,
    expect_pack: str | None = None,
) -> tuple[dict[str, Any], str, bool, list[str]]:
    """Re-derive ``summary.json`` from a run directory's own evidence.

    Reads only ``manifest.json``, ``episodes.jsonl`` and ``grader/results.json``
    — never the existing summary — and returns the derived summary, its
    rendering, whether that rendering is byte-identical to what is on disk, and
    any outputs whose digests have drifted from the manifest.

    Args:
        suite_keys: the pack's
            :meth:`~evals.core.pack.DatasetPack.summary_suite_keys`. Which
            dataset a directory belongs to comes from the command that
            recomputes it, not from the directory, so that a run directory
            that records no pack still reproduces.
        expect_pack: when given, the pack name the caller believes this
            directory holds.

    Raises:
        PackMismatch: if the manifest names a different pack. A manifest that
            names none is accepted.
    """
    manifest = json.loads((run_dir / "manifest.json").read_text(encoding="utf-8"))
    recorded = manifest.get("config", {}).get("dataset_pack")
    if expect_pack is not None and recorded is not None and recorded != expect_pack:
        raise PackMismatch(
            f"{run_dir} was produced by dataset pack {recorded!r}, "
            f"not {expect_pack!r}; recompute it with that pack"
        )
    episodes = [
        json.loads(line)
        for line in (run_dir / "episodes.jsonl").read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    cells = json.loads((run_dir / "grader" / "results.json").read_text(encoding="utf-8"))["cells"]
    summary = summarise(
        manifest=manifest, episodes=episodes, grader_cells=cells, suite_keys=suite_keys
    )
    rendered = render_summary(summary)

    from .manifest import verify_outputs

    existing = run_dir / "summary.json"
    matches = existing.is_file() and existing.read_text(encoding="utf-8") == rendered
    return summary, rendered, matches, verify_outputs(manifest, run_dir)


def _write_patch(run_dir: Path, outcome: EpisodeOutcome) -> None:
    """Store one episode's patch beside its record.

    Written even when empty, so the directory shows that the episode produced
    nothing rather than leaving a reader to wonder whether a file was lost.
    """
    path = (
        run_dir
        / "logs"
        / outcome.mode
        / str(outcome.repetition)
        / outcome.instance_id
        / f"attempt-{outcome.attempt}.patch"
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(outcome.patch, encoding="utf-8")
    for name, text in outcome.transcript:
        kept = path.parent / f"attempt-{outcome.attempt}" / name
        kept.parent.mkdir(parents=True, exist_ok=True)
        kept.write_text(text, encoding="utf-8")
