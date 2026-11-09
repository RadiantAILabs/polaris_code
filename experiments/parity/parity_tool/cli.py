"""Command line: run scripted sessions through two binaries and compare."""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from . import report
from .compare import AGREE, Comparison, compare
from .runner import Binary, RunResult, probe_rules_options, run_once
from .session import RunSpec, SessionError, load_sessions, select

DEFAULT_SESSIONS = Path(__file__).resolve().parent.parent / "sessions"


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="parity.py",
        description=(
            "Run scripted sessions through two agent binaries against a local fake "
            "model endpoint and report whether the model is sent the same rule text "
            "and the runs reach the same decisions and records."
        ),
    )
    parser.add_argument("binary_a", type=Path, help="the first binary (for example the old one)")
    parser.add_argument("binary_b", type=Path, help="the second binary (for example the new one)")
    parser.add_argument("--label-a", default="old", help="name for the first binary in the report")
    parser.add_argument("--label-b", default="new", help="name for the second binary in the report")
    parser.add_argument("--sessions", type=Path, default=DEFAULT_SESSIONS, help="folder of sessions")
    parser.add_argument(
        "--only",
        action="append",
        default=[],
        metavar="TEXT",
        help="run only runs whose id (session/run) contains TEXT; repeatable",
    )
    parser.add_argument("--out", type=Path, help="folder for the report and every run's files")
    parser.add_argument("--jobs", type=int, default=4, help="runs in parallel (default 4)")
    parser.add_argument("--timeout", type=float, default=180.0, help="seconds one run may take")
    parser.add_argument("--verbose", action="store_true", help="show every run and every stock difference")
    parser.add_argument("--list", action="store_true", help="list the runs and exit")
    return parser.parse_args(argv)


def _run_pair(
    spec: RunSpec, binaries: tuple[Binary, Binary], out_dir: Path, timeout: float
) -> Comparison:
    runs: list[RunResult] = [run_once(binary, spec, out_dir, timeout) for binary in binaries]
    comparison = compare(spec, (binaries[0].label, binaries[1].label), (runs[0], runs[1]))
    for run in runs:
        if run.run_dir:
            (run.run_dir / "requests.json").write_text(
                json.dumps(run.requests, indent=2), encoding="utf-8"
            )
    return comparison


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        sessions = load_sessions(args.sessions)
    except SessionError as error:
        print(f"parity: {error}", file=sys.stderr)
        return 2
    specs = select([spec for session in sessions for spec in session.run_specs()], args.only)
    if args.list:
        for spec in specs:
            print(f"{spec.id}\t{' '.join(spec.args)}")
        return 0
    if not specs:
        print("parity: no run matches", file=sys.stderr)
        return 2
    if args.label_a == args.label_b:
        print("parity: the two labels must differ", file=sys.stderr)
        return 2
    for path in (args.binary_a, args.binary_b):
        if not path.is_file():
            print(f"parity: {path}: not a file", file=sys.stderr)
            return 2

    out_dir = (args.out or Path(tempfile.mkdtemp(prefix="parity-"))).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    binaries = []
    for label, path in ((args.label_a, args.binary_a), (args.label_b, args.binary_b)):
        path = path.resolve()
        binaries.append(Binary(label, path, probe_rules_options(path, out_dir)))
    for binary in binaries:
        if not binary.rules_options:
            print(
                f"parity: {binary.label} has no --rules-arm option; rules options are left out "
                "of its command line",
                file=sys.stderr,
            )

    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        comparisons = list(
            pool.map(
                lambda spec: _run_pair(spec, (binaries[0], binaries[1]), out_dir, args.timeout),
                specs,
            )
        )

    names = (args.label_a, args.label_b)
    paths = (str(binaries[0].path), str(binaries[1].path))
    text = report.render(comparisons, names, paths, args.verbose, str(out_dir))
    (out_dir / "report.txt").write_text(text, encoding="utf-8")
    (out_dir / "report.json").write_text(
        json.dumps(report.to_json(comparisons, names, paths), indent=2), encoding="utf-8"
    )
    print(text, end="")
    return 0 if all(c.status == AGREE for c in comparisons) else 1
