"""An external SWE-bench Verified baseline: stock Claude Code, not polaris-code.

Each task runs in a fresh container from the same official image the
polaris-code packs use, hard-reset to the dataset row's base commit. Claude
Code gets the raw issue text as its only prompt (what polaris-code gets as
``-p``), edits ``/testbed``, and the patch is read back with the same
``git add -A`` / ``git diff --cached <base>`` convention and graded by the
same pinned official grader. Nothing from polaris-code runs: no binary, no
planner, no tools, no prompt.

Claude Code runs headless with its coding tools and unscoped Bash allowed
inside the throwaway container (no web tools); polaris-code episodes are
limited to the commands their run allows. Transcripts (Claude Code's JSON
result, stderr, and its session JSONL files) are kept under ``logs/<task>/``
with the provider key replaced.

Run from the polaris_code checkout::

    python3 -m evals.external.claude_code_swebench --instances a,b \\
        --output OUT --grader-python VENV/bin/python
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

from ..core.process import SubprocessRunner
from ..packs.swebench_verified.grader import Grader
from ..packs.swebench_verified.suite import Suite

TESTBED = "/testbed"
CONTAINER_CLAUDE = "/opt/claude-code/claude"
CONTAINER_HOME = "/root/claude-code-home"
MODE = "claude-code"
REDACTED_KEY = "[REDACTED:ANTHROPIC_API_KEY]"
KEY_VAR = "ANTHROPIC_API_KEY"
#: Claude Code's own coding tools, Bash unscoped: inside the throwaway
#: container this is how it runs unattended. Web tools are left out.
ALLOWED_TOOLS = "Bash,Read,Edit,Write,MultiEdit,Glob,Grep,TodoWrite,Task"
#: Bedrock mode passes these by name only, like the API key.
AWS_VARS = ("AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN")
#: The same rule polaris-code's edit steps carry, for a like-for-like baseline.
WORKSPACE_RULE = (
    "Work only with the checkout already in the workspace: do not download, install or fetch another "
    "version of the project or its upstream changes. Installing a missing third-party dependency is fine."
)


def _log_line(argv, finished) -> str:
    # Every argv carries only variable *names* for secrets (`--env NAME`), so
    # the line is safe to keep; the issue text is cut to keep logs short.
    shown = [a if len(a) < 200 else a[:60] + "…" for a in argv]
    state = "timed out" if finished.timed_out else f"exit {finished.returncode}"
    return f"{' '.join(shown)} -> {state}"


def run_task(instance, *, args, runner, key: str, logs: Path) -> dict:
    name = f"cc-baseline-{args.run_id}-{instance.instance_id}".replace("__", "-")
    task_logs = logs / instance.instance_id
    task_logs.mkdir(parents=True, exist_ok=True)
    commands: list[str] = []
    started = time.monotonic()
    record = {"instance_id": instance.instance_id, "patch": "", "termination": None, "error": None}

    def ex(argv, timeout=None, env=None):
        finished = runner.run(argv, timeout=timeout, env=env)
        commands.append(_log_line(argv, finished))
        return finished

    rt = args.container_runtime
    try:
        if not ex([rt, "image", "inspect", instance.image]).ok:
            ex([rt, "pull", instance.image])
        made = ex([rt, "create", "--name", name, "--workdir", TESTBED, "--entrypoint", "sleep", instance.image, "infinity"])
        if not made.ok:
            record.update(termination="infrastructure-failure", error=made.stderr.strip()[:400])
            return record
        for argv in (
            [rt, "start", name],
            [rt, "exec", name, "mkdir", "-p", CONTAINER_CLAUDE.rsplit("/", 1)[0], CONTAINER_HOME],
            [rt, "cp", args.claude_binary, f"{name}:{CONTAINER_CLAUDE}"],
            [rt, "exec", "--workdir", TESTBED, name, "git", "reset", "--hard", instance.base_commit],
            [rt, "exec", "--workdir", TESTBED, name, "git", "clean", "-fdx"],
        ):
            step = ex(argv)
            if not step.ok:
                record.update(termination="infrastructure-failure", error=f"setup failed: {step.stderr.strip()[:300]}")
                return record
        head = ex([rt, "exec", "--workdir", TESTBED, name, "git", "rev-parse", "HEAD"]).stdout.strip()
        if head != instance.base_commit:
            record.update(termination="infrastructure-failure", error=f"HEAD {head} is not {instance.base_commit}")
            return record

        secrets = {var: None for var in (AWS_VARS if args.bedrock else (KEY_VAR,))}
        env_names = {
            **secrets,  # passed through from this process's environment by name
            **({"CLAUDE_CODE_USE_BEDROCK": "1"} if args.bedrock else {}),
            "HOME": CONTAINER_HOME,
            "DISABLE_AUTOUPDATER": "1",
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        }
        argv = [rt, "exec", "--workdir", TESTBED]
        for var, value in env_names.items():
            argv += ["--env", var if value is None else f"{var}={value}"]
        argv += [
            name, CONTAINER_CLAUDE, "-p",
            instance.problem_statement + ("\n\n" + WORKSPACE_RULE if args.workspace_rule else ""),
            "--model", args.model, "--effort", args.effort,
            "--output-format", "json",
            "--permission-mode", "acceptEdits", "--allowedTools", ALLOWED_TOOLS,
        ]
        turn = ex(argv, timeout=args.episode_timeout_seconds, env={**os.environ, **({} if args.bedrock else {KEY_VAR: key})})

        kept = {"agent-stdout.json": turn.stdout or "", "agent-stderr.log": turn.stderr or ""}
        with tempfile.TemporaryDirectory() as holder:
            target = Path(holder) / "claude"
            if ex([rt, "cp", f"{name}:{CONTAINER_HOME}/.claude", str(target)]).ok:
                for path in sorted(target.rglob("*.jsonl")):
                    kept[f"sessions/{path.relative_to(target).as_posix()}"] = path.read_text(errors="replace")
        for rel, text in kept.items():
            out = task_logs / rel
            out.parent.mkdir(parents=True, exist_ok=True)
            for secret in [key] + [os.environ.get(v, "") for v in AWS_VARS[1:]]:
                if secret:
                    text = text.replace(secret, REDACTED_KEY if secret == key else "[REDACTED]")
            out.write_text(text, encoding="utf-8")

        result = None
        try:
            result = json.loads(turn.stdout)
        except (json.JSONDecodeError, TypeError):
            pass
        if isinstance(result, dict):
            record.update(
                num_turns=result.get("num_turns"),
                cost_usd=result.get("total_cost_usd"),
                usage=result.get("usage"),
                is_error=result.get("is_error"),
                subtype=result.get("subtype"),
            )
        if turn.timed_out:
            record.update(termination="timeout", error=f"exceeded {args.episode_timeout_seconds}s")
        elif result is None:
            record.update(termination="unreadable-result", error=(turn.stderr or "").strip()[-300:])
        else:
            record["termination"] = "agent-failed" if result.get("is_error") else "completed"

        if ex([rt, "exec", "--workdir", TESTBED, name, "git", "add", "-A"]).ok:
            diff = ex([rt, "exec", "--workdir", TESTBED, name, "git", "diff", "--cached", instance.base_commit])
            record["patch"] = diff.stdout if diff.ok else ""
        (task_logs / "attempt-1.patch").write_text(record["patch"], encoding="utf-8")
        return record
    finally:
        ex([rt, "rm", "-f", name])
        record["seconds"] = round(time.monotonic() - started, 1)
        (task_logs / "commands.log").write_text("\n".join(commands) + "\n", encoding="utf-8")


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--instances", required=True, help="comma-separated instance ids")
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--model", default="claude-sonnet-5")
    p.add_argument("--effort", default="medium", choices=["low", "medium", "high", "xhigh", "max"])
    p.add_argument("--claude-binary", default=shutil.which("claude"))
    p.add_argument("--container-runtime", default="docker")
    p.add_argument("--episode-timeout-seconds", type=int, default=1800)
    p.add_argument("--grade-timeout-seconds", type=int, default=1800)
    p.add_argument("--grader-python", default="python")
    p.add_argument("--max-workers", type=int, default=4, help="grader workers")
    p.add_argument("--concurrency", type=int, default=2, help="tasks run at once")
    p.add_argument("--run-id", default=None)
    p.add_argument("--bedrock", action="store_true", help="call Claude through Bedrock with AWS_* credentials")
    p.add_argument("--workspace-rule", action="store_true", help="append the checkout-only rule polaris-code uses")
    args = p.parse_args(argv)

    key = "" if args.bedrock else os.environ.get(KEY_VAR, "")
    if args.bedrock and not all(os.environ.get(v) for v in AWS_VARS[:3]):
        print("error: --bedrock needs AWS_REGION, AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY", file=sys.stderr)
        return 2
    if not args.bedrock and not key:
        print(f"error: {KEY_VAR} is not set", file=sys.stderr)
        return 2
    if not args.claude_binary:
        print("error: no claude binary; pass --claude-binary", file=sys.stderr)
        return 2
    args.claude_binary = str(Path(args.claude_binary).resolve())
    if args.output.exists() and any(args.output.iterdir()):
        print(f"error: {args.output} is not empty", file=sys.stderr)
        return 2
    args.output.mkdir(parents=True, exist_ok=True)
    args.run_id = args.run_id or args.output.name

    suite = Suite.load()
    ids = [i for i in args.instances.split(",") if i]
    instances = suite.select(ids)
    runner = SubprocessRunner()
    logs = args.output / "logs"
    version = runner.run([args.claude_binary, "--version"]).stdout.strip()
    (args.output / "manifest.json").write_text(json.dumps({
        "agent": "claude-code", "claude_code_version": version, "model": args.model,
        "effort": args.effort, "instances": ids, "episode_timeout_seconds": args.episode_timeout_seconds,
        "dataset": suite.dataset, "allowed_tools": ALLOWED_TOOLS, "bedrock": args.bedrock,
        "workspace_rule": args.workspace_rule,
    }, indent=2) + "\n")

    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = {pool.submit(run_task, inst, args=args, runner=runner, key=key, logs=logs): inst for inst in instances}
        for future in concurrent.futures.as_completed(futures):
            rec = future.result()
            records.append(rec)
            print(f"{MODE} {rec['instance_id']}: {rec['termination']} ({rec.get('seconds')}s, ${rec.get('cost_usd')})", flush=True)
            with (args.output / "episodes.jsonl").open("a") as fh:
                fh.write(json.dumps({k: v for k, v in rec.items() if k != "patch"} | {"patch_bytes": len(rec["patch"])}) + "\n")

    grader = Grader(runner, dataset=suite.dataset, max_workers=args.max_workers,
                    timeout=args.grade_timeout_seconds, python=args.grader_python)
    report = grader.grade(eval_run_id=args.run_id, mode=MODE, repetition=1,
                          patches={r["instance_id"]: r["patch"] for r in records},
                          instance_ids=ids, run_dir=args.output)
    grader.write_merged_predictions(args.output)
    summary = {
        "graded": report.graded, "note": report.note,
        "attempted": len(records), "resolved": len(report.resolved_ids),
        "resolved_ids": sorted(report.resolved_ids), "unresolved_ids": sorted(report.unresolved_ids),
        "empty_patch_ids": sorted(report.empty_patch_ids), "error_ids": sorted(report.error_ids),
        "cost_usd": round(sum(r.get("cost_usd") or 0 for r in records), 4),
        "terminations": {t: sum(1 for r in records if r["termination"] == t) for t in {r["termination"] for r in records}},
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
