#!/usr/bin/env python3
"""Decide whether a GitHub event needs the Rust and release CI jobs."""

import json
import os
import re
import subprocess
from pathlib import Path, PurePosixPath


def affects_build(path: str) -> bool:
    """Recognize workspace source, dependencies, and build or release tooling."""
    if path in {"Cargo.toml", "Cargo.lock"}:
        return True
    if path.startswith((".cargo/", ".github/workflows/", ".github/actions/")):
        return True
    if path.startswith("scripts/") and path.endswith((".py", ".sh")):
        return True
    # Workspace integration tests execute the external evaluation adapters.
    if path.startswith("evals/"):
        return not (
            path.startswith(("evals/docs/", "evals/results/"))
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


def requires_full_ci(event_name: str, event: dict, ref: str, repository: Path) -> bool:
    """Compare the whole push or PR; require full CI if comparison is unavailable."""
    if event_name == "workflow_dispatch" or ref.startswith("refs/tags/"):
        return True
    if event_name == "pull_request":
        pull_request = event.get("pull_request", {})
        base = pull_request.get("base", {}).get("sha", "")
        head = pull_request.get("head", {}).get("sha", "")
        separator = "..."
    elif event_name == "push":
        base, head = event.get("before", ""), event.get("after", "")
        separator = ".."
    else:
        return True
    if any(
        not re.fullmatch(r"[0-9a-f]{40,64}", sha) or not sha.strip("0")
        for sha in (base, head)
    ):
        return True
    result = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", f"{base}{separator}{head}"],
        cwd=repository,
        capture_output=True,
        check=False,
    )
    if result.returncode:
        print("Change comparison unavailable; requiring full CI.")
        return True
    paths = result.stdout.decode("utf-8", errors="surrogateescape").split("\0")
    return any(affects_build(path) for path in paths if path)


def main() -> None:
    """Write the full-CI decision to the preparation job's outputs."""
    event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
    required = requires_full_ci(
        os.environ["GITHUB_EVENT_NAME"], event, os.environ["GITHUB_REF"], Path.cwd()
    )
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as output:
        output.write(f"run_rust={str(required).lower()}\n")
    print(
        "Full CI required."
        if required
        else "Documentation or branding change; quick checks only."
    )


if __name__ == "__main__":
    main()
