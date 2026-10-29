"""``manifest.json``: everything a run is reproducible from, and its digests.

The manifest is schema ``polaris-code-harness-eval/v1``. It is written once
when the sweep starts, carrying the recorded configuration, the dataset pack's
identity and policy, the pinned suite revisions, the workspace commit,
the dirty-tree refusal result, the binary digest, the seeds, the timeouts, the
allowed environment-variable *names*, the host facts and the command line. It
is finalised once when the sweep ends, gaining only an ``outputs`` member of
content digests. Nothing else in the directory is ever rewritten.

What is deliberately absent: any environment-variable value, any prompt beyond
what the pack's own suite file already pins, and any model output. This is by
design: the manifest must stay safe to publish.

The ``pack`` member and ``config.dataset_pack`` are additive. Neither is read
by :func:`evals.core.runner.recompute`, so a run directory that lacks them
still reproduces its summary byte-for-byte.
"""

from __future__ import annotations

import hashlib
import json
import platform
import subprocess
from pathlib import Path
from typing import Any, Mapping, Sequence

from . import __version__
from .config import CONTAINER_BINARY_PATH, MANIFEST_SCHEMA, RunConfig
from .pack import DatasetPack, Suite

#: Files the core itself writes, whose digests a finalised manifest records.
#: ``summary.json`` is absent on purpose: it is derived *from* the manifest, so
#: including its digest here would make the pair mutually recursive and
#: un-reproducible. A pack adds its own artifacts —
#: SWE-bench's ``predictions.jsonl``, for instance — through
#: :meth:`~evals.core.pack.DatasetPack.digested_outputs`.
CORE_DIGESTED_OUTPUTS = ("instance_ids.txt", "episodes.jsonl", "grader/results.json")


def sha256_file(path: Path) -> str:
    """SHA-256 of a file, streamed so a large log costs no memory."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def repository_state(repo: Path) -> dict[str, Any]:
    """The commit and cleanliness of the workspace checkout.

    A dirty tree is recorded rather than tolerated silently: the runner refuses
    to start a *measured* run from one, because a result that cannot be tied to
    a commit is not evidence.
    """
    def git(*args: str) -> str | None:
        try:
            done = subprocess.run(  # noqa: S603 - fixed argv, no shell
                ["git", "-C", str(repo), *args],
                capture_output=True,
                check=False,
                text=True,
            )
        except FileNotFoundError:
            return None
        return done.stdout.strip() if done.returncode == 0 else None

    commit = git("rev-parse", "HEAD")
    status = git("status", "--porcelain")
    return {
        "path": "workspace",
        "commit": commit,
        "clean": None if status is None else status == "",
    }


def host_facts() -> dict[str, Any]:
    """Host OS and architecture, so local compute can be priced externally.

    Deliberately coarse. A hostname or a user name would be neither reproducible
    nor anyone's business.
    """
    return {
        "system": platform.system(),
        "release": platform.release(),
        "machine": platform.machine(),
        "python": platform.python_version(),
    }


def build(
    *,
    config: RunConfig,
    pack: DatasetPack,
    suite: Suite,
    selected_instance_ids: Sequence[str],
    binary_sha256: str | None,
    binary_bytes: int | None,
    repositories: Sequence[Mapping[str, Any]],
    command_line: Sequence[str],
    created_at: str,
    projected_cost_usd: float,
    episodes_planned: int,
) -> dict[str, Any]:
    """Assemble the manifest a run starts from."""
    revisions = suite.revisions()
    revisions["instance_ids"] = list(suite.instance_ids)
    return {
        "schema": MANIFEST_SCHEMA,
        "adapter_version": __version__,
        "created_at": created_at,
        "config": config.to_json(),
        "pack": {
            "name": pack.name,
            "version": pack.version,
            "policy": pack.policy(),
        },
        "suite": revisions,
        "selected_instance_ids": list(selected_instance_ids),
        "episodes_planned": episodes_planned,
        "projected_cost_usd": round(projected_cost_usd, 6),
        "binary": {
            "path": config.polaris_code_binary,
            "sha256": binary_sha256,
            "bytes": binary_bytes,
            "container_path": CONTAINER_BINARY_PATH,
        },
        "repositories": [dict(repo) for repo in repositories],
        "host": host_facts(),
        "command_line": list(command_line),
        "outputs": None,
    }


def finalise(
    manifest: Mapping[str, Any],
    run_dir: Path,
    *,
    extra_outputs: Sequence[str] = (),
) -> dict[str, Any]:
    """Return ``manifest`` with an ``outputs`` member of content digests.

    A file that the run never produced is recorded as ``null`` rather than
    omitted, so a reader can tell "absent" from "this manifest predates the
    field".

    Args:
        extra_outputs: the pack's own artifacts, from
            :meth:`~evals.core.pack.DatasetPack.digested_outputs`. Digested
            beside the core's so that no file a run directory relies on is left
            uncovered just because one dataset invented it.
    """
    outputs: dict[str, Any] = {}
    for relative in (*CORE_DIGESTED_OUTPUTS, *extra_outputs):
        path = run_dir / relative
        outputs[relative] = sha256_file(path) if path.is_file() else None
    return {**manifest, "outputs": outputs}


def render(manifest: Mapping[str, Any]) -> str:
    """Serialize a manifest deterministically."""
    return json.dumps(manifest, indent=2, sort_keys=True, ensure_ascii=False) + "\n"


def verify_outputs(manifest: Mapping[str, Any], run_dir: Path) -> list[str]:
    """Names of recorded outputs whose bytes no longer match their digest.

    Returns an empty list for an untampered directory. ``recompute`` reports
    the list rather than repairing it: an evidence record that has drifted from
    its own digests must be investigated, not quietly re-blessed.
    """
    recorded = manifest.get("outputs") or {}
    drifted: list[str] = []
    for relative, digest in recorded.items():
        path = run_dir / relative
        if digest is None:
            if path.is_file():
                drifted.append(relative)
            continue
        if not path.is_file() or sha256_file(path) != digest:
            drifted.append(relative)
    return drifted
