"""Run one agent binary through one scripted run and keep what it did."""

from __future__ import annotations

import fnmatch
import hashlib
import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

from .endpoint import Endpoint
from .network import NetworkGuard
from .session import RunSpec, expand

MODEL = "aws.bedrock/global.openai.gpt-5.4"

#: The command-line options of the rules experiment and whether each takes a
#: value. A binary that does not have them (the stock agent) is run without.
RULES_OPTIONS = {
    "--rules-arm": True,
    "--protect": True,
    "--test-command": True,
    "--run-record": True,
    "--check-timeout-secs": True,
    "--max-holds": True,
    "--no-give-up": False,
    "--rules-file": True,
    "--max-messages": True,
}

#: Names left out of the end-state comparison: what the agent keeps for itself.
ALWAYS_IGNORED = [".*", "*/.*", "__pycache__", "*/__pycache__", "*.pyc"]


def sandbox_env(
    home: Path, tmp: Path, endpoint_url: str | None = None, proxy_url: str | None = None
) -> dict[str, str]:
    """The only environment a binary is started with.

    The machine's own credentials and configuration are never inherited: the
    placeholder keys cannot sign for anything real, ``AWS_PROFILE`` is not
    passed, the instance metadata service is switched off, and the only
    endpoint is a local socket. When ``proxy_url`` is given, it is the proxy for
    every other request, so a build's own attempt to reach the internet (a price
    table, say) ends at a local refusal; see ``network.py``.
    """
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(home),
        "TMPDIR": str(tmp),
        "POLARIS_CONFIG_DIR": str(home),
        "POLARIS_CODE_PROVIDER": "bedrock",
        "POLARIS_CODE_MODEL": MODEL,
        "POLARIS_CODE_MODELS": MODEL,
        "POLARIS_CODE_NOTIFY": "off",
        "POLARIS_CODE_PROMPT_CACHE": "0",
        "AWS_REGION": "eu-west-2",
        "AWS_EC2_METADATA_DISABLED": "true",
        "AWS_MAX_ATTEMPTS": "1",
        "AWS_ACCESS_KEY_ID": "placeholder-key-id",
        "AWS_SECRET_ACCESS_KEY": "placeholder-secret",
        "NO_PROXY": "127.0.0.1,localhost",
    }
    if endpoint_url:
        # The Responses route reads only the general variable; Converse reads
        # the service one. Both point at the local socket.
        env["AWS_ENDPOINT_URL"] = endpoint_url
        env["AWS_ENDPOINT_URL_BEDROCK_RUNTIME"] = endpoint_url
    if proxy_url:
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"):
            env[name] = proxy_url
            env[name.lower()] = proxy_url
        env["no_proxy"] = env["NO_PROXY"]
    return env


@dataclass
class Binary:
    """An agent binary under test."""

    label: str
    path: Path
    #: Whether it has the rules experiment's options (found with ``--help``).
    rules_options: bool = True
    #: Arguments added to the end of every run's command line, after the session's.
    extra_args: list[str] = field(default_factory=list)


def probe_rules_options(path: Path, scratch: Path) -> bool:
    """Whether the binary lists ``--rules-arm`` in its help."""
    home = scratch / "probe-home"
    tmp = scratch / "probe-tmp"
    home.mkdir(parents=True, exist_ok=True)
    tmp.mkdir(parents=True, exist_ok=True)
    done = subprocess.run(
        [str(path), "--help"],
        # Nothing listens on the discard port, so even here a stray request fails.
        env=sandbox_env(home, tmp, None, "http://127.0.0.1:9"),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=60,
        cwd=scratch,
    )
    return "--rules-arm" in (done.stdout + done.stderr)


def split_rules_options(args: list[str]) -> tuple[list[str], list[str]]:
    """Split ``args`` into the arguments to keep and the rules options dropped."""
    kept: list[str] = []
    dropped: list[str] = []
    index = 0
    while index < len(args):
        arg = args[index]
        name = arg.split("=", 1)[0]
        if name in RULES_OPTIONS:
            dropped.append(arg)
            if RULES_OPTIONS[name] and "=" not in arg and index + 1 < len(args):
                index += 1
                dropped.append(args[index])
        else:
            kept.append(arg)
        index += 1
    return kept, dropped


def _digest(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def tree_digests(root: Path, ignore: list[str]) -> dict[str, str]:
    """Relative path -> content hash of every file under ``root``."""
    patterns = [*ALWAYS_IGNORED, *ignore]
    found: dict[str, str] = {}
    for current, dirs, files in os.walk(root):
        rel_dir = Path(current).relative_to(root)
        dirs[:] = sorted(
            d
            for d in dirs
            if not any(fnmatch.fnmatch(str(rel_dir / d), p) for p in patterns)
        )
        for name in sorted(files):
            rel = str(rel_dir / name)
            if any(fnmatch.fnmatch(rel, p) for p in patterns):
                continue
            path = Path(current) / name
            if path.is_symlink() or not path.is_file():
                continue
            found[rel] = _digest(path)
    return found


@dataclass
class RunResult:
    """Everything one binary did in one run."""

    label: str
    argv: list[str]
    #: Rules options left out because the binary does not have them.
    dropped_options: list[str]
    #: The process exit code; ``None`` when it did not exit by itself.
    exit_code: int | None
    timed_out: bool
    seconds: float
    #: The requests the endpoint saw: ``{"path", "body", "auxiliary"}`` each.
    requests: list[dict]
    #: Requests that arrived after the script ran out.
    beyond_script: int
    #: The run record, or ``None`` if the binary wrote none.
    record: dict | None
    stdout: str
    stderr: str
    #: Workspace path -> content hash at the end.
    final_files: dict[str, str]
    #: Run-specific strings and what they stand for in a comparison.
    replacements: list[tuple[str, str]] = field(default_factory=list)
    #: Where the run's files were kept.
    run_dir: Path | None = None
    #: Set when the binary could not be started.
    start_error: str | None = None
    #: Requests the binary made to anywhere but the model endpoint, as
    #: ``"<method> <target>"``. All were refused.
    network_attempts: list[str] = field(default_factory=list)


def run_once(binary: Binary, spec: RunSpec, out_dir: Path, timeout: float) -> RunResult:
    """Start ``binary`` on ``spec`` against a fresh endpoint and collect the run."""
    run_dir = out_dir / "runs" / spec.session.name / spec.name / binary.label
    if run_dir.exists():
        shutil.rmtree(run_dir)
    workspace = run_dir / "workspace"
    home = run_dir / "home"
    tmp = run_dir / "tmp"
    shutil.copytree(spec.session.workspace, workspace)
    home.mkdir(parents=True)
    tmp.mkdir()
    record_path = run_dir / "record.json"

    mapping = {"workspace": str(workspace), "session": str(spec.session.path)}
    session_args = expand(spec.args, mapping)
    dropped: list[str] = []
    if binary.rules_options:
        if "--run-record" not in session_args:
            session_args += ["--run-record", str(record_path)]
        else:
            record_path = Path(session_args[session_args.index("--run-record") + 1])
    else:
        session_args, dropped = split_rules_options(session_args)
    argv = [
        str(binary.path),
        str(workspace),
        "-p",
        spec.session.task,
        "--output-format",
        "json",
        "--allow-edit",
        "--allow-command",
        "*",
        "--allow-tool",
        "*",
        "--max-turns",
        str(spec.session.max_turns),
        *session_args,
        *binary.extra_args,
    ]

    script = expand(spec.session.replies, mapping)
    started = time.monotonic()
    exit_code: int | None = None
    timed_out = False
    start_error: str | None = None
    with Endpoint(script) as endpoint, NetworkGuard() as guard:
        env = sandbox_env(home, tmp, endpoint.url, guard.url)
        env.update(expand(spec.env, mapping))
        stdout_path = run_dir / "stdout"
        stderr_path = run_dir / "stderr"
        with stdout_path.open("wb") as stdout, stderr_path.open("wb") as stderr:
            try:
                child = subprocess.Popen(
                    argv,
                    env=env,
                    cwd=run_dir,
                    stdin=subprocess.DEVNULL,
                    stdout=stdout,
                    stderr=stderr,
                )
            except OSError as error:
                child = None
                start_error = str(error)
            if child is not None:
                try:
                    exit_code = child.wait(timeout=timeout)
                except subprocess.TimeoutExpired:
                    timed_out = True
                    # Only this child, by its handle: nothing is signalled by id.
                    child.kill()
                    child.wait()
        requests = list(endpoint.requests)
        beyond = endpoint.beyond_script
        port = endpoint.port
        attempts = list(guard.attempts)
    seconds = time.monotonic() - started

    record: dict | None = None
    try:
        record = json.loads(record_path.read_bytes())
    except (OSError, ValueError):
        record = None

    replacements = _replacements(run_dir, workspace, port)
    return RunResult(
        label=binary.label,
        argv=argv,
        dropped_options=dropped,
        exit_code=exit_code,
        timed_out=timed_out,
        seconds=seconds,
        requests=requests,
        beyond_script=beyond,
        record=record,
        stdout=stdout_path.read_text(encoding="utf-8", errors="replace"),
        stderr=stderr_path.read_text(encoding="utf-8", errors="replace"),
        final_files=tree_digests(workspace, spec.session.ignore_files),
        replacements=replacements,
        run_dir=run_dir,
        start_error=start_error,
        network_attempts=attempts,
    )


def _replacements(run_dir: Path, workspace: Path, port: int) -> list[tuple[str, str]]:
    """The strings that differ between runs by construction, longest first."""
    pairs = [
        (str(workspace), "<workspace>"),
        (str(workspace.resolve()), "<workspace>"),
        (str(run_dir / "tmp"), "<tmp>"),
        (str((run_dir / "tmp").resolve()), "<tmp>"),
        (str(run_dir / "home"), "<home>"),
        (str((run_dir / "home").resolve()), "<home>"),
        (str(run_dir), "<run>"),
        (str(run_dir.resolve()), "<run>"),
        (f"127.0.0.1:{port}", "127.0.0.1:<port>"),
    ]
    unique = dict.fromkeys(pairs)
    return sorted(unique, key=lambda pair: -len(pair[0]))
