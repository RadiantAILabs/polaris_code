"""Drives one fixture end to end: copy, invoke the binary once, collect evidence.

A route that never calls a model (``topology-edit``/``topology-statement`` under
the ``deterministic`` composition policy, over one of the five registrations
this build admits with no test runner and no model provider wired — see
``evals/README.md``) needs the binary to have *booted* with
``ANTHROPIC_API_KEY`` set, but calls no provider: nothing here or in the graphs
those routes reach ever calls ``ModelRegistry::llm``. So when the environment
does not already carry a real key, this module supplies the same literal
deliberately invalid placeholder, ``not-a-real-key`` — enough to pass the
boot-time presence check, spending nothing. A ``conversational`` route runs
the real agent loop, which does call a model; that route is marked
``requires_live_model`` in ``expect.toml`` and is skipped, not failed, when no
real key is available.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path
from typing import Any

from .expect import FixtureExpect
from .spec import build_argv

#: Deliberately invalid placeholder. Anything reaching a real provider call
#: with this value fails loudly and
#: distinctly (an auth error), so a route that was supposed to need no model
#: cannot silently "succeed" against a real network call by accident.
PLACEHOLDER_API_KEY = "not-a-real-key"

#: Wall-clock budget for one turn. Every route this runner can invoke without a
#: real key resolves in well under a second against the real graphs (no model
#: call, no test runner); a route that does call a model gets the same budget
#: an interactive turn would reasonably use.
TURN_TIMEOUT_SECONDS = 120


@dataclasses.dataclass(frozen=True)
class FixtureRun:
    """Everything one fixture's run produced, for checking and reporting."""

    expect: FixtureExpect
    workspace_dir: Path
    argv: list[str]
    returncode: int | None
    stdout: str
    stderr: str
    report: dict[str, Any] | None
    tree_unchanged: bool
    skipped: bool
    skip_reason: str | None


def _tree_digest(root: Path) -> str:
    """A stable digest of every regular file's relative path and bytes.

    Walks in sorted order so the digest does not depend on filesystem
    enumeration order, and excludes ``.polaris`` — the agent's own state
    directory (``crates/polaris_code_core/src/state.rs``), which every turn touches by design and which
    is not part of "did the work change the tree".
    """
    hasher = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(root)
        if relative.parts and relative.parts[0] == ".polaris":
            continue
        hasher.update(str(relative).encode())
        hasher.update(path.read_bytes())
    return hasher.hexdigest()


def _has_real_key() -> bool:
    key = os.environ.get("ANTHROPIC_API_KEY", "")
    return bool(key) and key != PLACEHOLDER_API_KEY


def run_fixture(
    fixture_dir: Path,
    expect: FixtureExpect,
    *,
    binary: Path,
    composition_policy: str = "deterministic",
    workdir_parent: Path | None = None,
) -> FixtureRun:
    """Copies the fixture's workspace to a fresh directory and runs one turn.

    The workspace is always a fresh copy: the polaris-code state directory
    (``<working_dir>/.polaris``) accumulates evidence across turns, and a fresh
    copy is what keeps the deterministic-selector cold-start facts this
    package's fixtures are calibrated against (see
    ``evals/dynamic_topology/tests/test_checks.py`` and
    ``docs/topology-selection.md``'s ranking-from-outcomes note) reproducible
    run over run. Call :func:`cleanup` on the result when done with it.
    """
    if expect.route.requires_live_model and not _has_real_key():
        tmp_root = Path(tempfile.mkdtemp(prefix=f"dyntopo-{expect.name}-", dir=workdir_parent))
        shutil.copytree(fixture_dir / "workspace", tmp_root, dirs_exist_ok=True)
        return FixtureRun(
            expect=expect,
            workspace_dir=tmp_root,
            argv=[],
            returncode=None,
            stdout="",
            stderr="",
            report=None,
            tree_unchanged=True,
            skipped=True,
            skip_reason=(
                "requires a real ANTHROPIC_API_KEY (conversational route calls the model); "
                "set one and pass --include-conversational to run it"
            ),
        )

    tmp_root = Path(tempfile.mkdtemp(prefix=f"dyntopo-{expect.name}-", dir=workdir_parent))
    workspace_dir = tmp_root / "workspace"
    shutil.copytree(fixture_dir / "workspace", workspace_dir)

    before = _tree_digest(workspace_dir)
    argv = [str(binary), *build_argv(expect.route, default_ref=expect.name), str(workspace_dir)]

    env = dict(os.environ)
    env.setdefault("ANTHROPIC_API_KEY", PLACEHOLDER_API_KEY)
    env["POLARIS_CODE_COMPOSITION_POLICY"] = composition_policy

    try:
        completed = subprocess.run(
            argv,
            capture_output=True,
            text=True,
            timeout=TURN_TIMEOUT_SECONDS,
            env=env,
            check=False,
        )
        returncode = completed.returncode
        stdout = completed.stdout
        stderr = completed.stderr
    except subprocess.TimeoutExpired as error:
        returncode = None
        stdout = error.stdout.decode() if isinstance(error.stdout, bytes) else (error.stdout or "")
        stderr = f"timed out after {TURN_TIMEOUT_SECONDS}s"

    after = _tree_digest(workspace_dir)

    report: dict[str, Any] | None = None
    if stdout.strip():
        try:
            report = json.loads(stdout)
        except json.JSONDecodeError:
            report = None

    return FixtureRun(
        expect=expect,
        workspace_dir=workspace_dir,
        argv=argv,
        returncode=returncode,
        stdout=stdout,
        stderr=stderr,
        report=report,
        tree_unchanged=before == after,
        skipped=False,
        skip_reason=None,
    )


def cleanup(run: FixtureRun) -> None:
    """Removes a fixture run's scratch directory."""
    shutil.rmtree(run.workspace_dir.parent, ignore_errors=True)
