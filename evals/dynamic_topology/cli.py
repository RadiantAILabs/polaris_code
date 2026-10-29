"""Command-line entry point: ``python3 -m evals.dynamic_topology run [...]``.

Thin by design — ``evals/run.sh`` is the documented entry point and mostly just
locates a built binary and execs into this module. Everything that decides
what happens lives in :mod:`expect`, :mod:`spec`, :mod:`runner`,
:mod:`checks` and :mod:`report`, each independently testable.
"""

from __future__ import annotations

import argparse
import os
import sys
from pathlib import Path

from . import checks, expect as expect_mod, report as report_mod, runner as runner_mod

#: Repository root, derived from this file's own path
#: (``evals/dynamic_topology/cli.py`` is three levels under it) rather than the
#: process's current directory, so `run.sh` can be invoked from anywhere.
REPO_ROOT = Path(__file__).resolve().parents[2]

DEFAULT_TASKS_DIR = REPO_ROOT / "evals" / "tasks"
DEFAULT_RESULTS_DIR = REPO_ROOT / "target" / "eval-reports" / "dynamic-topology"

#: Build locations this CLI checks, in order, when ``--binary`` and
#: ``POLARIS_CODE_BIN`` are both absent: ``target/debug``, then
#: ``target/release``.
_DEFAULT_BINARY_CANDIDATES = (
    REPO_ROOT / "target" / "debug" / "polaris-code",
    REPO_ROOT / "target" / "release" / "polaris-code",
)


def _default_binary() -> Path | None:
    from_env = os.environ.get("POLARIS_CODE_BIN")
    if from_env:
        return Path(from_env)
    for candidate in _DEFAULT_BINARY_CANDIDATES:
        if candidate.is_file():
            return candidate
    return None


def _parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(prog="evals.dynamic_topology")
    subparsers = parser.add_subparsers(dest="command", required=True)

    run = subparsers.add_parser("run", help="run one or more fixtures and write a results table")
    run.add_argument(
        "fixtures",
        nargs="*",
        help="fixture directory names to run (default: every fixture under evals/tasks/)",
    )
    run.add_argument("--binary", type=Path, default=None, help="path to the polaris-code binary")
    run.add_argument("--tasks-dir", type=Path, default=DEFAULT_TASKS_DIR)
    run.add_argument("--results-dir", type=Path, default=DEFAULT_RESULTS_DIR)
    run.add_argument(
        "--composition-policy",
        default="deterministic",
        choices=("deterministic", "model", "gates"),
        help=(
            "propagated as POLARIS_CODE_COMPOSITION_POLICY. `model` makes one real, "
            "billed chooser call per topology-route fixture — see evals/README.md"
        ),
    )
    run.add_argument(
        "--include-conversational",
        action="store_true",
        help="also run fixtures whose route needs a real model (spends money; needs a real key)",
    )
    run.add_argument("--keep-tmp", action="store_true", help="do not delete per-fixture scratch dirs")
    return parser.parse_args(argv)


def _run(args: argparse.Namespace) -> int:
    binary = args.binary or _default_binary()
    if binary is None or not binary.is_file():
        print(
            "no polaris-code binary found; build one and pass --binary or set POLARIS_CODE_BIN",
            file=sys.stderr,
        )
        return 2

    fixture_dirs = expect_mod.discover(args.tasks_dir)
    if args.fixtures:
        wanted = set(args.fixtures)
        fixture_dirs = [path for path in fixture_dirs if path.name in wanted]
        missing = wanted - {path.name for path in fixture_dirs}
        if missing:
            print(f"unknown fixture(s): {', '.join(sorted(missing))}", file=sys.stderr)
            return 2

    outcomes: list[report_mod.FixtureOutcome] = []
    failures = 0
    for fixture_dir in fixture_dirs:
        loaded = expect_mod.load(fixture_dir)
        if loaded.route.requires_live_model and not args.include_conversational:
            run = runner_mod.FixtureRun(
                expect=loaded,
                workspace_dir=fixture_dir / "workspace",
                argv=[],
                returncode=None,
                stdout="",
                stderr="",
                report=None,
                tree_unchanged=True,
                skipped=True,
                skip_reason="conversational route; pass --include-conversational to run it",
            )
            outcomes.append(
                report_mod.FixtureOutcome(
                    run=run,
                    outcome=checks.CheckResult(True, "skipped"),
                    plan=checks.CheckResult(True, "skipped"),
                )
            )
            print(f"[skip] {loaded.name}: conversational route not included this run")
            continue

        run = runner_mod.run_fixture(
            fixture_dir,
            loaded,
            binary=binary,
            composition_policy=args.composition_policy,
        )
        try:
            if run.skipped:
                outcome_result = checks.CheckResult(True, "skipped")
                plan_result = checks.CheckResult(True, "skipped")
                print(f"[skip] {loaded.name}: {run.skip_reason}")
            else:
                outcome_result = checks.check_outcome(
                    loaded.outcome,
                    run.workspace_dir,
                    run.report,
                    tree_unchanged=run.tree_unchanged,
                )
                plan_result = checks.check_plan(loaded.plan, run.report)
                status = "ok" if (outcome_result.ok and plan_result.ok) else "FAIL"
                print(f"[{status}] {loaded.name}: outcome={outcome_result.ok} plan={plan_result.ok}")
                if not outcome_result.ok:
                    print(f"         outcome: {outcome_result.detail}")
                if not plan_result.ok:
                    print(f"         plan:    {plan_result.detail}")
                if not (outcome_result.ok and plan_result.ok):
                    failures += 1
            outcomes.append(
                report_mod.FixtureOutcome(run=run, outcome=outcome_result, plan=plan_result)
            )
        finally:
            if not run.skipped and not args.keep_tmp:
                runner_mod.cleanup(run)

    path = report_mod.write(
        outcomes, composition_policy=args.composition_policy, results_dir=args.results_dir
    )
    print(f"wrote {path}")
    return 1 if failures else 0


def main(argv: list[str] | None = None) -> int:
    args = _parse_args(sys.argv[1:] if argv is None else argv)
    if args.command == "run":
        return _run(args)
    raise AssertionError(f"unhandled command {args.command!r}")  # pragma: no cover


if __name__ == "__main__":
    sys.exit(main())
