"""An external SWE-bench Verified baseline: stock Codex CLI on Bedrock, not polaris-code.

Like ``claude_code_swebench``, each task runs in a fresh container
from the official image, hard-reset to the base commit. Codex gets the raw
issue text as its only prompt, edits ``/testbed``, and the patch is read back
with ``git add -A`` / ``git diff --cached <base>`` and graded by the same
pinned official grader.

Codex talks to Bedrock through its built-in ``amazon-bedrock-runtime``
provider with a short-lived Bedrock bearer token, minted on the host for each
task and passed into the container by variable name only. Codex's own sandbox
is off: the throwaway container is the sandbox, as it is for the Claude Code
baseline. Transcripts (the ``--json`` event stream, stderr, and Codex's
session files) are kept under ``logs/<task>/`` with the token replaced.

Run from the polaris_code checkout::

    python3 -m evals.external.codex_swebench --instances a,b \\
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
from .claude_code_swebench import WORKSPACE_RULE, _log_line

TESTBED = "/testbed"
CONTAINER_CODEX_DIR = "/opt/codex"
CONTAINER_HOME = "/root/codex-home"
MODE = "codex"
TOKEN_VAR = "AWS_BEARER_TOKEN_BEDROCK"
REDACTED = "[REDACTED:AWS_BEARER_TOKEN_BEDROCK]"
PROVIDER = "amazon-bedrock-runtime"
#: The price list polaris-code's shared price store reads; prices are looked up
#: by model id at start-up, never kept here.
PRICE_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
MINT = 'from aws_bedrock_token_generator import provide_token; print(provide_token(region="{region}"))'


def config_toml(args) -> str:
    return (
        f'model = "{args.model}"\n'
        f'model_provider = "{PROVIDER}"\n'
        f'model_reasoning_effort = "{args.effort}"\n'
        f"[model_providers.{PROVIDER}.aws]\n"
        f'region = "{args.region}"\n'
    )


def load_price(url: str, model: str) -> dict:
    """USD per million tokens for ``model``, from the price list at ``url`` (a URL or a file path)."""
    import urllib.request

    if "://" in url:
        with urllib.request.urlopen(url, timeout=30) as response:
            table = json.load(response)
    else:
        table = json.loads(Path(url).read_text())
    entry = table.get(model)
    if entry is None:
        sys.exit(f"error: {model} is not in the price list {url}; pass --price-url")
    per_token = {
        "input": entry["input_cost_per_token"],
        "cached": entry.get("cache_read_input_token_cost", entry["input_cost_per_token"]),
        "cache_write": entry.get("cache_creation_input_token_cost", entry["input_cost_per_token"]),
        "output": entry["output_cost_per_token"],
    }
    return {k: v * 1e6 for k, v in per_token.items()}


def usage_cost(usage: dict, price: dict) -> float:
    """``input_tokens`` includes both cache reads and cache writes; the rest is plain input."""
    cached = usage.get("cached_input_tokens", 0)
    written = usage.get("cache_write_input_tokens", 0)
    fresh = max(usage.get("input_tokens", 0) - cached - written, 0)
    return (fresh * price["input"] + cached * price["cached"] + written * price["cache_write"]
            + usage.get("output_tokens", 0) * price["output"]) / 1e6


def read_events(stdout: str) -> dict:
    """Sum the usage of every completed turn and count the shell commands run."""
    usage: dict[str, int] = {}
    commands = turns = 0
    failed = None
    for line in (stdout or "").splitlines():
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        if kind == "turn.completed":
            turns += 1
            for k, v in (event.get("usage") or {}).items():
                if isinstance(v, int):
                    usage[k] = usage.get(k, 0) + v
        elif kind == "turn.failed":
            failed = (event.get("error") or {}).get("message", "turn failed")
        elif kind == "item.completed" and (event.get("item") or {}).get("type") == "command_execution":
            commands += 1
    return {"usage": usage, "commands": commands, "turns": turns, "failed": failed}


def run_task(instance, *, args, runner, logs: Path) -> dict:
    name = f"codex-baseline-{args.run_id}-{instance.instance_id}".replace("__", "-")
    task_logs = logs / instance.instance_id
    task_logs.mkdir(parents=True, exist_ok=True)
    commands: list[str] = []
    started = time.monotonic()
    record = {"instance_id": instance.instance_id, "patch": "", "termination": None, "error": None}
    token = ""

    def ex(argv, timeout=None, env=None):
        finished = runner.run(argv, timeout=timeout, env=env)
        commands.append(_log_line(argv, finished))
        return finished

    rt = args.container_runtime
    try:
        minted = runner.run([args.token_python, "-c", MINT.format(region=args.region)])
        token = minted.stdout.strip()
        if not minted.ok or not token:
            record.update(termination="infrastructure-failure", error="could not mint a Bedrock token")
            return record
        if not ex([rt, "image", "inspect", instance.image]).ok:
            ex([rt, "pull", instance.image])
        made = ex([rt, "create", "--name", name, "--workdir", TESTBED, "--entrypoint", "sleep", instance.image, "infinity"])
        if not made.ok:
            record.update(termination="infrastructure-failure", error=made.stderr.strip()[:400])
            return record
        with tempfile.TemporaryDirectory() as holder:
            cfg = Path(holder) / "config.toml"
            cfg.write_text(config_toml(args), encoding="utf-8")
            steps = [
                [rt, "start", name],
                [rt, "exec", name, "mkdir", "-p", CONTAINER_CODEX_DIR, CONTAINER_HOME],
                *([rt, "cp", str(path), f"{name}:{CONTAINER_CODEX_DIR}/{path.name}"] for path in args.codex_files),
                [rt, "cp", str(cfg), f"{name}:{CONTAINER_HOME}/config.toml"],
                [rt, "exec", "--workdir", TESTBED, name, "git", "reset", "--hard", instance.base_commit],
                [rt, "exec", "--workdir", TESTBED, name, "git", "clean", "-fdx"],
            ]
            for argv in steps:
                step = ex(argv)
                if not step.ok:
                    record.update(termination="infrastructure-failure", error=f"setup failed: {step.stderr.strip()[:300]}")
                    return record
        head = ex([rt, "exec", "--workdir", TESTBED, name, "git", "rev-parse", "HEAD"]).stdout.strip()
        if head != instance.base_commit:
            record.update(termination="infrastructure-failure", error=f"HEAD {head} is not {instance.base_commit}")
            return record

        argv = [rt, "exec", "--workdir", TESTBED,
                "--env", TOKEN_VAR,  # passed through from this process's environment by name
                "--env", f"AWS_REGION={args.region}", "--env", f"CODEX_HOME={CONTAINER_HOME}",
                name, f"{CONTAINER_CODEX_DIR}/codex", "exec", "--json", "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox", "-C", TESTBED,
                instance.problem_statement + ("\n\n" + WORKSPACE_RULE if args.workspace_rule else "")]
        turn = ex(argv, timeout=args.episode_timeout_seconds, env={**os.environ, TOKEN_VAR: token})

        kept = {"agent-events.jsonl": turn.stdout or "", "agent-stderr.log": turn.stderr or ""}
        with tempfile.TemporaryDirectory() as holder:
            target = Path(holder) / "home"
            if ex([rt, "cp", f"{name}:{CONTAINER_HOME}/sessions", str(target)]).ok:
                for path in sorted(target.rglob("*.jsonl")):
                    kept[f"sessions/{path.relative_to(target).as_posix()}"] = path.read_text(errors="replace")
        for rel, text in kept.items():
            out = task_logs / rel
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_text(text.replace(token, REDACTED), encoding="utf-8")

        seen = read_events(turn.stdout)
        record.update(usage=seen["usage"], num_commands=seen["commands"], num_turns=seen["turns"],
                      cost_usd=round(usage_cost(seen["usage"], args.price), 5))
        if turn.timed_out:
            record.update(termination="timeout", error=f"exceeded {args.episode_timeout_seconds}s")
        elif seen["failed"]:
            record.update(termination="agent-failed", error=seen["failed"][:300])
        elif not seen["turns"]:
            record.update(termination="unreadable-result", error=(turn.stderr or "").strip()[-300:])
        else:
            record["termination"] = "completed"

        if ex([rt, "exec", "--workdir", TESTBED, name, "git", "add", "-A"]).ok:
            diff = ex([rt, "exec", "--workdir", TESTBED, name, "git", "diff", "--cached", instance.base_commit])
            record["patch"] = diff.stdout if diff.ok else ""
        (task_logs / "attempt-1.patch").write_text(record["patch"], encoding="utf-8")
        return record
    finally:
        ex([rt, "rm", "-f", name])
        record["seconds"] = round(time.monotonic() - started, 1)
        text = "\n".join(commands) + "\n"
        (task_logs / "commands.log").write_text(text.replace(token, REDACTED) if token else text, encoding="utf-8")


def main(argv=None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--instances", required=True, help="comma-separated instance ids")
    p.add_argument("--output", required=True, type=Path)
    p.add_argument("--model", required=True, help="Bedrock model id")
    p.add_argument("--effort", default="medium", choices=["minimal", "low", "medium", "high", "xhigh"])
    p.add_argument("--region", default="eu-west-2")
    p.add_argument("--codex-binary", default=shutil.which("codex"))
    p.add_argument("--token-python", default=sys.executable,
                   help="a python with aws-bedrock-token-generator installed")
    p.add_argument("--container-runtime", default="docker")
    p.add_argument("--episode-timeout-seconds", type=int, default=1800)
    p.add_argument("--grade-timeout-seconds", type=int, default=1800)
    p.add_argument("--grader-python", default="python")
    p.add_argument("--max-workers", type=int, default=4, help="grader workers")
    p.add_argument("--concurrency", type=int, default=2, help="tasks run at once")
    p.add_argument("--run-id", default=None)
    p.add_argument("--price-url", default=PRICE_URL, help="LiteLLM-format price list, URL or file")
    p.add_argument("--workspace-rule", action="store_true", help="append the checkout-only rule polaris-code uses")
    args = p.parse_args(argv)

    if not args.codex_binary:
        print("error: no codex binary; pass --codex-binary", file=sys.stderr)
        return 2
    binary = Path(args.codex_binary).resolve()
    # The standalone release ships helper binaries beside `codex`; copy them all.
    args.codex_files = sorted(p for p in binary.parent.iterdir() if p.is_file())
    if args.output.exists() and any(args.output.iterdir()):
        print(f"error: {args.output} is not empty", file=sys.stderr)
        return 2
    args.output.mkdir(parents=True, exist_ok=True)
    args.run_id = args.run_id or args.output.name

    suite = Suite.load()
    ids = [i for i in args.instances.split(",") if i]
    instances = suite.select(ids)
    args.price = load_price(args.price_url, args.model)
    runner = SubprocessRunner()
    logs = args.output / "logs"
    version = runner.run([str(binary), "--version"]).stdout.strip()
    (args.output / "manifest.json").write_text(json.dumps({
        "agent": "codex", "codex_version": version, "model": args.model, "provider": PROVIDER,
        "region": args.region, "effort": args.effort, "instances": ids,
        "episode_timeout_seconds": args.episode_timeout_seconds, "dataset": suite.dataset,
        "price_per_mtok": args.price, "price_url": args.price_url, "workspace_rule": args.workspace_rule,
    }, indent=2) + "\n")

    records = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.concurrency) as pool:
        futures = {pool.submit(run_task, inst, args=args, runner=runner, logs=logs): inst for inst in instances}
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
