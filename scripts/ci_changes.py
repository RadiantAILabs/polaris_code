#!/usr/bin/env python3
"""Decide which CI jobs a GitHub event needs.

`run_rust` selects the Rust and release jobs. `experiments_only` is true when
every changed path is under `experiments/`; those changes are checked by the
separate experiments workflow, so this workflow skips its other jobs too.
"""

import json
import os
import re
import subprocess
from pathlib import Path, PurePosixPath


def is_experiment(path: str) -> bool:
    """Recognize files owned by the experiments workflow."""
    return path.startswith("experiments/")


def affects_build(path: str) -> bool:
    """Recognize workspace source, dependencies, and build or release tooling."""
    if path in {"Cargo.toml", "Cargo.lock"}:
        return True
    # Experiment crates are workspace members, so the workspace jobs still build
    # and test them whenever shared source, manifests or the lockfile change.
    if is_experiment(path):
        return False
    if path.startswith((".cargo/", ".github/workflows/", ".github/actions/")):
        return True
    if path.startswith("scripts/") and path.endswith((".py", ".sh")):
        return True
    # Workspace integration tests execute the external evaluation adapters. The
    # reproduction kits are run by hand and no test reads them.
    if path.startswith("evals/"):
        return not (
            path.startswith(("evals/docs/", "evals/results/", "evals/reproduce/"))
            or PurePosixPath(path).name in {"README.md", "AGENTS.md", "CLAUDE.md"}
        )
    if PurePosixPath(path).name.startswith("rust-toolchain"):
        return True
    parts = PurePosixPath(path).parts
    return (
        len(parts) >= 3
        and parts[0] == "crates"
        and (
            parts[2] in {"src", "tests", "examples", "benches", "ontologies"}
            or parts[-1] in {"Cargo.toml", "Cargo.lock", "build.rs"}
            or path.endswith(".rs")
        )
    )


def changed_paths(
    event_name: str, event: dict, ref: str, repository: Path
) -> list[str] | None:
    """List the whole push or PR's changed paths; `None` when full CI is required."""
    if event_name == "workflow_dispatch" or ref.startswith("refs/tags/"):
        return None
    if event_name == "pull_request":
        pull_request = event.get("pull_request", {})
        base = pull_request.get("base", {}).get("sha", "")
        head = pull_request.get("head", {}).get("sha", "")
        separator = "..."
    elif event_name == "push":
        base, head = event.get("before", ""), event.get("after", "")
        separator = ".."
    else:
        return None
    if any(
        not re.fullmatch(r"[0-9a-f]{40,64}", sha) or not sha.strip("0")
        for sha in (base, head)
    ):
        return None
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", f"{base}{separator}{head}"],
        cwd=repository,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        print("Change comparison unavailable; requiring full CI.")
        return None
    paths = result.stdout.decode("utf-8", errors="surrogateescape").split("\0")
    return [path for path in paths if path]


def requires_full_ci(event_name: str, event: dict, ref: str, repository: Path) -> bool:
    """Compare the whole push or PR; require full CI if comparison is unavailable."""
    paths = changed_paths(event_name, event, ref, repository)
    return paths is None or any(affects_build(path) for path in paths)


def only_experiments(event_name: str, event: dict, ref: str, repository: Path) -> bool:
    """Whether every changed path is an experiment file; false when unknown or empty."""
    paths = changed_paths(event_name, event, ref, repository)
    return bool(paths) and all(is_experiment(path) for path in paths)


def main() -> None:
    """Write the CI decisions to the preparation job's outputs."""
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    arguments = (
        os.environ["GITHUB_EVENT_NAME"], event, os.environ["GITHUB_REF"], Path.cwd()
    )
    required = requires_full_ci(*arguments)
    experiments = only_experiments(*arguments)
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"run_rust={str(required).lower()}\n")
        output.write(f"experiments_only={str(experiments).lower()}\n")
    if required:
        print("Full CI required.")
    elif experiments:
        print("Experiments change; the experiments workflow checks it.")
    else:
        print("Documentation or branding change; quick checks only.")


if __name__ == "__main__":
    main()
