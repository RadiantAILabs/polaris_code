"""The command line every dataset pack shares.

A pack's ``__main__`` calls :func:`main` with its own
:class:`~evals.core.pack.DatasetPack`, and gets these commands:

``run``        the sweep. One documented invocation covers curated, freeform
               and both; a full sweep, a replayed plumbing
               demonstration and a fast iteration run all differ from each
               other only in arguments. There is one runner on purpose.
``recompute``  re-derive ``summary.json`` from an existing run directory and
               prove it is byte-identical.
``preflight``  check the prerequisites — binary, container runtime, provider
               key, plus whatever the pack adds — without running or spending
               anything.

The pack contributes three things to this parser and nothing else: extra ``run``
flags (:meth:`~evals.core.pack.DatasetPack.add_run_arguments`), extra preflight
rows (:meth:`~evals.core.pack.DatasetPack.preflight_checks`), and any
dataset-only subcommand (:meth:`~evals.core.pack.DatasetPack.add_commands`,
dispatched through :meth:`~evals.core.pack.DatasetPack.handle_command`). That
is why SWE-bench's ``pin`` is not in this module.

Exit codes: ``0`` success, ``1`` the command ran and its answer was negative
(a drifted summary, a failed preflight), ``2`` a usage or configuration error.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sys
from pathlib import Path
from typing import Any, Mapping, Sequence

from .budget import BudgetExceeded
from .config import (
    API_KEY_VAR,
    ARMS,
    DEFAULT_MAX_OUTPUT_TOKENS,
    DEFAULT_MAX_TURNS,
    DOMAINS,
    AXIS_COMPOSITION,
    AXIS_GRAPH_SEARCH,
    AXIS_TOPOLOGY,
    COMPOSITION_POLICIES,
    DEFAULT_EPISODE_TIMEOUT_SECONDS,
    DEFAULT_GRADE_TIMEOUT_SECONDS,
    DEFAULT_MAX_WORKERS,
    EXPERIMENT_AXES,
    MODES,
    PORTABLE_BINARY_PATH,
    RunConfig,
    lean_passthrough_names,
    normalise_composition_policies,
    normalise_domain_selection,
    normalise_modes,
    normalise_topologies,
)
from .pack import DatasetPack, Suite
from .plan_history import PLAN_HISTORY_VAR
from .process import REPLAY, SUBPROCESS, ReplayRunner, SubprocessRunner
from .profiles import profile_names, resolve as resolve_profile
from .runner import DirtyTree, PackMismatch, RunDirectoryExists, recompute, run_sweep

def _positive_int(raw: str) -> int:
    """Parse a flag value the binary would accept: a positive integer."""
    try:
        value = int(raw)
    except ValueError:
        raise argparse.ArgumentTypeError(f"expected a positive integer, got {raw!r}") from None
    if value < 1:
        raise argparse.ArgumentTypeError(f"expected a positive integer, got {raw!r}")
    return value


def build_parser(pack: DatasetPack) -> argparse.ArgumentParser:
    """The argument parser, which is also the command's documentation."""
    parser = argparse.ArgumentParser(
        prog=f"python -m evals.packs.{pack.name.replace('-', '_')}",
        description=(
            f"Evaluate polaris-code on the {pack.name} dataset pack, once per "
            "graph-search, topology or composition cell, through the shipped "
            "non-interactive entry point."
        ),
    )
    commands = parser.add_subparsers(dest="command", required=True)

    run = commands.add_parser("run", help="run a sweep into a fresh run directory")
    run.add_argument("--model", default="anthropic/claude-sonnet-5", help="POLARIS_CODE_MODEL value")
    run.add_argument("--modes", default=",".join(MODES), help=f"comma-separated subset of {','.join(MODES)}")
    run.add_argument(
        "--experiment-axis",
        choices=EXPERIMENT_AXES,
        default=AXIS_GRAPH_SEARCH,
        help="agent dimension to vary; topology and composition hold graph search fixed",
    )
    run.add_argument(
        "--topologies",
        default=None,
        help="comma-separated registered topology ids; required for --experiment-axis topology",
    )
    run.add_argument(
        "--composition-policies",
        default=None,
        help=(
            "comma-separated composition policies (of "
            f"{','.join(COMPOSITION_POLICIES)}); required for --experiment-axis composition"
        ),
    )
    run.add_argument(
        "--graph-search-mode",
        choices=MODES,
        default="both",
        help="fixed graph-search surface for a topology or composition sweep (default: both)",
    )
    run.add_argument(
        "--composition-policy",
        choices=COMPOSITION_POLICIES,
        default="deterministic",
        help=(
            "composition policy held fixed for every episode when "
            "composition is not the varying axis"
        ),
    )
    run.add_argument(
        "--topology-intent",
        choices=("fast", "careful", "thorough", "answer"),
        default="answer",
        help="task profile submitted through topology work (default: answer)",
    )
    run.add_argument(
        "--domain",
        choices=DOMAINS,
        default="none",
        help=(
            "domain this run's gate-plan turns select "
            "(default: none, the generic plan unmodified)"
        ),
    )
    run.add_argument(
        "--arm",
        choices=ARMS,
        default="generic",
        help=(
            "plan/notes treatment the domain above selects (default: generic). "
            "generic-notes, expert and experience all require --domain other "
            "than none; composed also requires --composition-policies gates-model; "
            "loop/lean (one model conversation) "
            "and fine/fine-ctx (staged generic proposal) require --domain none"
        ),
    )
    run.add_argument(
        "--keep-transcripts",
        action="store_true",
        help="keep each episode's agent output and session files under logs/ (holds the issue text)",
    )
    run.add_argument(
        "--max-turns",
        type=_positive_int,
        default=DEFAULT_MAX_TURNS,
        help=f"agent loop turn limit per turn (POLARIS_CODE_MAX_TURNS, default {DEFAULT_MAX_TURNS})",
    )
    run.add_argument(
        "--max-output-tokens",
        type=_positive_int,
        default=DEFAULT_MAX_OUTPUT_TOKENS,
        help=f"output token limit per model request (POLARIS_CODE_MAX_OUTPUT_TOKENS, default {DEFAULT_MAX_OUTPUT_TOKENS})",
    )
    run.add_argument("--repetitions", type=int, default=None, help="complete sweeps; the evidence bar needs 3")
    run.add_argument("--order-seed", type=int, default=1729, help="seed for the per-cell mode-order shuffle")
    run.add_argument("--analysis-seed", type=int, default=1729, help="seed for the bootstrap")
    run.add_argument(
        "--instances",
        default=None,
        help=(
            "comma-separated subset of pinned instance ids. A subset is a "
            "plumbing run: the denominator stays fixed, so no cell can score."
        ),
    )
    run.add_argument(
        "--iteration-profile",
        default=None,
        choices=profile_names(pack.iteration_profiles()),
        help=(
            "run a named, pinned fast configuration instead of the whole suite. "
            "Its name is recorded in the manifest and every artefact it writes "
            "is labelled not-evidence; it can never reach a scored conclusion."
        ),
    )
    run.add_argument("--max-workers", type=int, default=DEFAULT_MAX_WORKERS)
    run.add_argument(
        "--episode-workers",
        type=int,
        default=1,
        help="episodes to run at once (default 1: one after another)",
    )
    run.add_argument("--episode-timeout-seconds", type=int, default=None)
    run.add_argument("--grade-timeout-seconds", type=int, default=DEFAULT_GRADE_TIMEOUT_SECONDS)
    run.add_argument(
        "--api-cost-ceiling-usd",
        type=float,
        default=pack.default_api_cost_ceiling_usd(),
        help="hard API-cost ceiling for this run",
    )
    run.add_argument("--output", required=True, help="the immutable run directory to create")
    run.add_argument(
        "--plan-history",
        default=os.environ.get(PLAN_HISTORY_VAR),
        metavar="PATH",
        help=(
            "append officially graded gate-plan outcomes to this JSONL file "
            f"(default: ${PLAN_HISTORY_VAR}; disabled when unset)"
        ),
    )
    run.add_argument(
        "--polaris-code",
        default=PORTABLE_BINARY_PATH,
        help=(
            "path to the portable release binary to evaluate "
            f"(default: {PORTABLE_BINARY_PATH}; build it with scripts/build-portable-binary.sh)"
        ),
    )
    run.add_argument("--container-runtime", default="docker")
    run.add_argument(
        "--allow-edit",
        action=argparse.BooleanOptionalAction,
        default=True,
        help="grant edit_file to the unattended run; without it every patch is empty",
    )
    run.add_argument("--allow-command", action="append", default=[], help="shell-policy glob, repeatable")
    run.add_argument(
        "--topology-work",
        action="store_true",
        help="submit each instance to the binary's topology-work affordance",
    )
    run.add_argument("--allow-dirty", action="store_true", help="permit a run from a dirty checkout")
    run.add_argument(
        "--replay",
        default=None,
        help=(
            "replay a recorded script instead of executing anything. Produces a "
            "plumbing demonstration, never a measurement."
        ),
    )
    run.add_argument("--quiet", action="store_true")
    pack.add_run_arguments(run)

    again = commands.add_parser("recompute", help="re-derive summary.json and prove it reproduces")
    again.add_argument("run_dir")
    again.add_argument("--write", action="store_true", help="write the derived summary if it is missing")

    pre = commands.add_parser("preflight", help="check prerequisites without running anything")
    pre.add_argument(
        "--polaris-code",
        default=PORTABLE_BINARY_PATH,
        help=(
            "path to the portable release binary to check "
            f"(default: {PORTABLE_BINARY_PATH})"
        ),
    )
    pre.add_argument("--container-runtime", default="docker")
    pack.add_preflight_arguments(pre)

    pack.add_commands(commands)
    return parser


def main(pack: DatasetPack, argv: Sequence[str] | None = None) -> int:
    """Entry point for one pack. Returns the exit code rather than exiting."""
    args = build_parser(pack).parse_args(list(argv) if argv is not None else None)
    try:
        suite = pack.load_suite()
    except pack.refusals as err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    if args.command == "run":
        return _run(pack, args, suite, list(argv) if argv is not None else sys.argv[1:])
    if args.command == "recompute":
        return _recompute(pack, args)
    if args.command == "preflight":
        return _preflight(pack, args, suite)
    return pack.handle_command(args.command, args, suite)


def _run(
    pack: DatasetPack,
    args: argparse.Namespace,
    suite: Suite,
    command_line: Sequence[str],
) -> int:
    """The ``run`` command."""
    complaint = pack.check_run_arguments(args, suite)
    if complaint is not None:
        print(f"error: {complaint}", file=sys.stderr)
        return 2

    try:
        normalise_domain_selection(args.domain, args.arm)
    except ValueError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    if args.experiment_axis != AXIS_TOPOLOGY and args.topologies is not None:
        print("error: --topologies requires --experiment-axis topology", file=sys.stderr)
        return 2
    if args.experiment_axis != AXIS_COMPOSITION and args.composition_policies is not None:
        print(
            "error: --composition-policies requires --experiment-axis composition",
            file=sys.stderr,
        )
        return 2

    if args.experiment_axis == AXIS_GRAPH_SEARCH:
        try:
            modes = normalise_modes([m for m in args.modes.split(",") if m])
        except ValueError as err:
            print(f"error: {err}", file=sys.stderr)
            return 2
        graph_search_mode = None
    elif args.experiment_axis == AXIS_TOPOLOGY:
        if args.modes != ",".join(MODES):
            print(
                "error: --modes cannot cross graph-search and topology axes; "
                "use --graph-search-mode to hold one graph-search value fixed",
                file=sys.stderr,
            )
            return 2
        if args.topology_work:
            print(
                "error: --topology-work is the legacy directed treatment; "
                "--experiment-axis topology selects and pins each topology itself",
                file=sys.stderr,
            )
            return 2
        if args.topologies is None:
            print("error: --experiment-axis topology requires --topologies", file=sys.stderr)
            return 2
        try:
            modes = normalise_topologies([m for m in args.topologies.split(",") if m])
        except ValueError as err:
            print(f"error: {err}", file=sys.stderr)
            return 2
        graph_search_mode = args.graph_search_mode

    else:
        if args.modes != ",".join(MODES):
            print(
                "error: --modes cannot cross graph-search and composition axes; "
                "use --graph-search-mode to hold one graph-search value fixed",
                file=sys.stderr,
            )
            return 2
        if args.topology_work:
            print(
                "error: --topology-work is the legacy directed treatment; "
                "--experiment-axis composition submits topology work itself, "
                "with topology selection left automatic",
                file=sys.stderr,
            )
            return 2
        if args.composition_policies is None:
            print(
                "error: --experiment-axis composition requires --composition-policies",
                file=sys.stderr,
            )
            return 2
        try:
            modes = normalise_composition_policies(
                [m for m in args.composition_policies.split(",") if m]
            )
        except ValueError as err:
            print(f"error: {err}", file=sys.stderr)
            return 2
        graph_search_mode = args.graph_search_mode

    if args.arm == "composed" and (
        args.experiment_axis != AXIS_COMPOSITION or modes != ("gates-model",)
    ):
        print(
            "error: --arm composed requires --experiment-axis composition "
            "--composition-policies gates-model",
            file=sys.stderr,
        )
        return 2

    # A profile only ever *fills in* the three fields it pins. Overriding one by
    # hand would leave the run carrying a name whose meaning it no longer has,
    # and two people's "rust-smoke-7 runs" would stop being comparable — which
    # is the single thing a named profile exists to provide.
    profile = (
        resolve_profile(pack.iteration_profiles(), args.iteration_profile)
        if args.iteration_profile
        else None
    )
    if profile is not None:
        clashing = [
            flag
            for flag, value in (
                ("--instances", args.instances),
                ("--repetitions", args.repetitions),
                ("--episode-timeout-seconds", args.episode_timeout_seconds),
            )
            if value is not None
        ]
        if clashing:
            print(
                f"error: --iteration-profile {profile.name} already pins "
                + ", ".join(clashing)
                + "; drop the flag, or drop --iteration-profile and state the whole "
                "configuration by hand",
                file=sys.stderr,
            )
            return 2

    # `is None` rather than falsiness throughout: `--repetitions 0` is a value
    # the caller stated and must still be refused below, not quietly read as
    # "unset" and replaced with the default.
    if profile is not None:
        instances = profile.instance_ids
        repetitions = profile.repetitions
        episode_timeout = profile.episode_timeout_seconds
    else:
        instances = tuple(i for i in (args.instances or "").split(",") if i)
        repetitions = 1 if args.repetitions is None else args.repetitions
        episode_timeout = (
            DEFAULT_EPISODE_TIMEOUT_SECONDS
            if args.episode_timeout_seconds is None
            else args.episode_timeout_seconds
        )
    if repetitions < 1:
        print("error: --repetitions must be at least 1", file=sys.stderr)
        return 2

    output = Path(args.output)
    process_runner = (
        ReplayRunner.from_file(Path(args.replay)) if args.replay else SubprocessRunner()
    )
    config = RunConfig(
        eval_run_id=output.name,
        dataset_pack=pack.name,
        model=args.model,
        modes=modes,
        repetitions=repetitions,
        order_seed=args.order_seed,
        analysis_seed=args.analysis_seed,
        instance_ids=instances,
        episode_timeout_seconds=episode_timeout,
        grade_timeout_seconds=args.grade_timeout_seconds,
        max_workers=args.max_workers,
        api_cost_ceiling_usd=args.api_cost_ceiling_usd,
        polaris_code_binary=args.polaris_code,
        container_runtime=args.container_runtime,
        allow_edit=args.allow_edit,
        allow_command=tuple(args.allow_command),
        topology_work=(
            args.topology_work or args.experiment_axis in (AXIS_TOPOLOGY, AXIS_COMPOSITION)
        ),
        process_runner=REPLAY if args.replay else SUBPROCESS,
        iteration_profile=None if profile is None else profile.name,
        axis=args.experiment_axis,
        graph_search_mode=graph_search_mode,
        composition_policy=args.composition_policy,
        topology_intent=args.topology_intent,
        max_turns=args.max_turns,
        max_output_tokens=args.max_output_tokens,
        keep_transcripts=args.keep_transcripts,
        domain=args.domain,
        arm=args.arm,
        lean_env_names=lean_passthrough_names(os.environ),
    )

    try:
        result = run_sweep(
            config=config,
            pack=pack,
            suite=suite,
            output=output,
            process_runner=process_runner,
            command_line=command_line,
            repositories=_repository_paths(),
            api_key=os.environ.get(API_KEY_VAR),
            options=args,
            require_clean_tree=not args.allow_dirty and not args.replay,
            plan_history=None if not args.plan_history else Path(args.plan_history),
            progress=(lambda _m: None) if args.quiet else (lambda m: print(m, file=sys.stderr)),
            episode_workers=args.episode_workers,
        )
    except (RunDirectoryExists, DirtyTree, BudgetExceeded, *pack.refusals) as err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    summary = result.summary
    banner = not_evidence_banner(summary)
    if banner is not None:
        # Never suppressed by --quiet. --quiet silences progress, and this is
        # not progress: it is the sentence that has to stay attached to the
        # numbers printed immediately below it.
        print(banner, file=sys.stderr)
    print(json.dumps(run_report(summary, run_dir=result.run_dir), indent=2, sort_keys=True))
    return 0


def run_report(summary: Mapping[str, Any], *, run_dir: Path) -> dict[str, Any]:
    """What ``run`` prints on stdout.

    Carries ``not_evidence`` beside the figures rather than only the
    ``conclusion``, so a tool that consumes this has one field to check and a
    person who copies a fragment of it takes the disclaimer along.
    """
    return {
        "run_dir": str(run_dir),
        "conclusion": summary["conclusion"],
        "not_evidence": summary.get("not_evidence"),
        "iteration_profile": summary.get("iteration_profile"),
        "complete": summary["complete"],
        "cells": summary["cells"],
        "totals": summary["totals"],
    }


def not_evidence_banner(summary: Mapping[str, Any]) -> str | None:
    """The line a run that is not a measurement prints before its numbers.

    ``None`` for a measured run: a warning that appears every time is a warning
    nobody reads, and this one has to still work when it matters.
    """
    label = summary.get("not_evidence")
    if label is None:
        return None
    return f"{label}: {summary['conclusion_note']}"


def _recompute(pack: DatasetPack, args: argparse.Namespace) -> int:
    """The ``recompute`` command."""
    run_dir = Path(args.run_dir)
    try:
        summary, rendered, matches, drifted = recompute(
            run_dir,
            suite_keys=pack.summary_suite_keys(),
            expect_pack=pack.name,
        )
    except FileNotFoundError as err:
        print(f"error: {err}", file=sys.stderr)
        return 2
    except PackMismatch as err:
        print(f"error: {err}", file=sys.stderr)
        return 2

    if drifted:
        print(
            "error: these outputs no longer match the digests in manifest.json: "
            + ", ".join(drifted),
            file=sys.stderr,
        )
        return 1
    if matches:
        print(f"summary.json reproduces byte-for-byte from {run_dir}")
        print(f"conclusion: {summary['conclusion']}")
        banner = not_evidence_banner(summary)
        if banner is not None:
            print(banner, file=sys.stderr)
        return 0
    if args.write and not (run_dir / "summary.json").is_file():
        (run_dir / "summary.json").write_text(rendered, encoding="utf-8")
        print(f"wrote {run_dir / 'summary.json'}")
        return 0
    print("error: the derived summary differs from summary.json on disk", file=sys.stderr)
    return 1


def _preflight(pack: DatasetPack, args: argparse.Namespace, suite: Suite) -> int:
    """The ``preflight`` command: report every prerequisite, then verdict."""
    checks: list[tuple[str, bool, str]] = []

    binary = Path(args.polaris_code) if args.polaris_code else None
    checks.append((
        "polaris-code binary",
        bool(binary and binary.is_file() and os.access(binary, os.X_OK)),
        str(binary) if binary else "not given (--polaris-code)",
    ))
    runtime = shutil.which(args.container_runtime)
    checks.append(("container runtime", runtime is not None, runtime or f"{args.container_runtime} not on PATH"))
    checks.append((f"{API_KEY_VAR}", bool(os.environ.get(API_KEY_VAR)), "set" if os.environ.get(API_KEY_VAR) else "unset"))
    checks.extend(pack.preflight_checks(args, suite))

    width = max(len(name) for name, _, _ in checks)
    for name, ok, detail in checks:
        print(f"{'ok  ' if ok else 'MISS'} {name.ljust(width)}  {detail}")
    failed = [name for name, ok, _ in checks if not ok]
    if failed:
        print(f"\nnot ready for a measured sweep: {', '.join(failed)}", file=sys.stderr)
        return 1
    print("\nready for a measured sweep")
    return 0


def _repository_paths() -> list[Path]:
    """The workspace checkout whose commit belongs in a manifest."""
    root = Path(__file__).resolve().parents[2]
    return [root] if (root / ".git").exists() else []
