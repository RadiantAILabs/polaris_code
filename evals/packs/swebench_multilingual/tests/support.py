"""Fixtures shared by this pack's tests.

Every sweep here runs the real :class:`SwebenchMultilingualPack` against a
synthetic suite, so what is under test is the pack the shipped command line
uses and not a stand-in.

Two kinds of suite appear here. :func:`tiny_suite` builds a synthetic three-task
suite so a test can assert behaviour without carrying 43 rows of real issue
text; it deliberately keeps the real denominator of 43, because the fixed
denominator is the property most of these tests exist to protect. Tests that
are *about* the pinned list load the real one instead.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Iterable, Sequence

from ....core.config import AXIS_GRAPH_SEARCH, RunConfig
from ....core.process import REPLAY, ReplayRunner
from ....core.runner import SweepResult, recompute, run_sweep
from ..pack import SwebenchMultilingualPack
from ..replay_script import ScriptedEpisode, build
from ..suite import Instance, Suite

#: Instance ids used by the synthetic suite, in ascending order.
TINY_IDS = ("aaa__one-1", "bbb__two-2", "ccc__three-3")


def tiny_suite(instance_ids: Sequence[str] = TINY_IDS) -> Suite:
    """A synthetic suite whose rows are cheap but whose denominator is real."""
    return Suite(
        dataset="SWE-bench/SWE-bench_Multilingual",
        dataset_revision="846e647b9f33c0b51b739d005d13d85493c9af09",
        dataset_split="test",
        dataset_file="data/test-00000-of-00001.parquet",
        dataset_file_sha256="0" * 64,
        dataset_file_bytes=1,
        task_repo="SWE-bench/swe-bench-multilingual-tasks",
        task_repo_revision="6e08cbcd763c869abd7aae4bcb9e1b3991180777",
        grader_repo="SWE-bench/SWE-bench",
        grader_revision="02e7a74ffd0b707aab73d203fe87bdc7c76afc8e",
        language="Rust",
        repositories=("example/one",),
        instances=tuple(
            Instance(
                instance_id=instance_id,
                repo="example/one",
                base_commit=f"base{index}" + "0" * 34,
                version=str(index),
                image=f"swebench/sweb.eval.x86_64.{instance_id}:latest",
                problem_statement=f"issue text for {instance_id}\nwith a second line\n",
            )
            for index, instance_id in enumerate(instance_ids)
        ),
    )


def config(
    *,
    output: Path,
    modes: Sequence[str] = ("curated", "freeform", "both"),
    repetitions: int = 1,
    instance_ids: Sequence[str] = (),
    ceiling: float = 150.0,
    binary: str = "/nonexistent/polaris-code",
    allow_edit: bool = True,
    allow_command: Sequence[str] = (),
    topology_work: bool = False,
    process_runner: str = REPLAY,
    iteration_profile: str | None = None,
    episode_timeout_seconds: int = 1800,
    axis: str = AXIS_GRAPH_SEARCH,
    graph_search_mode: str | None = None,
    composition_policy: str = "deterministic",
) -> RunConfig:
    """A run configuration for a replayed sweep into ``output``.

    ``process_runner`` is the string the manifest *records*, which is not the
    same thing as the runner a test drives the sweep with. Passing
    ``SUBPROCESS`` here while still replaying is how a test exercises what a
    measured run would write, with only the kernel call substituted — the same
    substitution the whole suite is built on.
    """
    return RunConfig(
        eval_run_id=output.name,
        dataset_pack=SwebenchMultilingualPack.name,
        model="anthropic/claude-sonnet-4-6",
        modes=tuple(modes),
        repetitions=repetitions,
        order_seed=1729,
        analysis_seed=1729,
        instance_ids=tuple(instance_ids),
        episode_timeout_seconds=episode_timeout_seconds,
        grade_timeout_seconds=1800,
        max_workers=4,
        api_cost_ceiling_usd=ceiling,
        polaris_code_binary=binary,
        container_runtime="docker",
        allow_edit=allow_edit,
        allow_command=tuple(allow_command),
        topology_work=topology_work,
        process_runner=process_runner,
        iteration_profile=iteration_profile,
        axis=axis,
        graph_search_mode=graph_search_mode,
        composition_policy=composition_policy,
    )


def scripted_sweep(
    *,
    suite: Suite,
    eval_run_id: str,
    modes: Sequence[str] = ("curated", "freeform", "both"),
    repetitions: int = 1,
    resolved: Iterable[tuple[str, str]] = (),
    patched: Iterable[tuple[str, str]] | None = None,
    cost_per_episode: float | None = 0.01,
) -> list[ScriptedEpisode]:
    """One scripted episode per cell of the matrix.

    ``resolved`` and ``patched`` are ``(instance_id, mode)`` pairs. An instance
    that is resolved is also patched unless ``patched`` says otherwise, because
    the official grader cannot resolve an empty patch.
    """
    resolved_pairs = set(resolved)
    patched_pairs = set(patched) if patched is not None else set(resolved_pairs)
    episodes: list[ScriptedEpisode] = []
    for repetition in range(1, repetitions + 1):
        for mode in modes:
            for index, instance in enumerate(suite.instances):
                pair = (instance.instance_id, mode)
                episodes.append(
                    ScriptedEpisode(
                        instance_id=instance.instance_id,
                        mode=mode,
                        repetition=repetition,
                        patch=(
                            f"diff --git a/src.rs b/src.rs\n@@\n-old\n+new for {mode}\n"
                            if pair in patched_pairs
                            else ""
                        ),
                        input_tokens=1000 + index,
                        output_tokens=50 + index,
                        cache_read_tokens=10,
                        cache_creation_tokens=5,
                        estimated_cost_usd=cost_per_episode,
                        resolved=pair in resolved_pairs,
                    )
                )
    return episodes


def replay_sweep(
    *,
    tmp: Path,
    suite: Suite,
    episodes: Sequence[ScriptedEpisode],
    run_config: RunConfig,
    **kwargs: object,
) -> tuple[SweepResult, ReplayRunner]:
    """Run one replayed sweep and hand back both the result and the runner.

    The runner is returned because ``runner.issued`` — the exact argv a
    measured sweep would have executed — is what most of these tests assert on.
    """
    runner = ReplayRunner(build(suite=suite, eval_run_id=run_config.eval_run_id, scripted=episodes))
    result = run_sweep(
        config=run_config,
        pack=SwebenchMultilingualPack(),
        suite=suite,
        output=tmp,
        process_runner=runner,
        command_line=["run", "--output", str(tmp)],
        require_clean_tree=False,
        now=lambda: "2000-01-01T00:00:00Z",
        **kwargs,  # type: ignore[arg-type]
    )
    return result, runner


def pack_recompute(run_dir: Path):
    """Recompute a run directory exactly as this pack's command line does.

    The core takes the summary's suite keys from the pack, so a test that
    recomputed with the core defaults would derive a different — and correctly
    different — summary from the one the sweep wrote.
    """
    pack = SwebenchMultilingualPack()
    return recompute(run_dir, suite_keys=pack.summary_suite_keys(), expect_pack=pack.name)


def issued_lines(runner: ReplayRunner) -> list[str]:
    """Every issued command as one joined line, for substring assertions."""
    return [" ".join(argv) for argv in runner.issued]


def read_jsonl(path: Path) -> list[dict]:
    """Read a JSON-lines file."""
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
