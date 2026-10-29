"""Checks a fixture's outcome and chosen plan against its ``expect.toml``.

Both checks are pure functions over already-collected evidence — a completed
subprocess result, file bytes on disk, the parsed ``--output-format json``
report — so this module never itself starts the binary or a model. That is
what makes :mod:`evals.dynamic_topology.tests.test_checks` able to exercise
every checker deterministically against a hand-built report and a hand-solved
workspace copy.
"""

from __future__ import annotations

import dataclasses
import os
import subprocess
from pathlib import Path
from typing import Any

from .expect import OutcomeSpec, PlanSpec


@dataclasses.dataclass(frozen=True)
class CheckResult:
    """The verdict of one check, plus the detail a results table shows."""

    ok: bool
    detail: str


def check_outcome(
    outcome: OutcomeSpec,
    workspace_dir: Path,
    report: dict[str, Any] | None,
    *,
    tree_unchanged: bool | None = None,
) -> CheckResult:
    """Checks whether the fixture's work landed, per its declared ``check``.

    ``tree_unchanged`` is computed by the caller (a runner has the fixture's
    before/after tree hashes; a deterministic test over a single already-solved
    copy has nothing to compare, and passes ``None`` to skip that half of an
    ``answer_contains`` check).
    """
    if outcome.check == "command":
        return _check_command(outcome, workspace_dir)
    if outcome.check == "file_contains":
        return _check_file_contains(outcome, workspace_dir)
    if outcome.check == "answer_contains":
        return _check_answer_contains(outcome, report, tree_unchanged=tree_unchanged)
    raise ValueError(f"unknown outcome check {outcome.check!r}")  # pragma: no cover


def _command_env() -> dict[str, str]:
    """The environment an outcome command runs under.

    Strips ``CARGO_TARGET_DIR`` (which a developer may set to a shared
    directory) because a fixture workspace is an unrelated standalone crate
    with the same name in every copy. Reusing a shared target directory lets
    cargo's fingerprint cache answer a `cargo test` in a just-copied *solved*
    tree with the stale binary built for the *unsolved* tree moments before,
    which is exactly backwards for a checker whose whole job is telling the two
    apart. Each fixture copy gets its own default `<workspace>/target` instead.
    """
    env = dict(os.environ)
    env.pop("CARGO_TARGET_DIR", None)
    return env


def _check_command(outcome: OutcomeSpec, workspace_dir: Path) -> CheckResult:
    if not outcome.command:
        raise ValueError("a command outcome needs a non-empty command")
    cwd = workspace_dir / outcome.cwd
    try:
        completed = subprocess.run(
            list(outcome.command),
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=300,
            env=_command_env(),
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as error:
        return CheckResult(False, f"{' '.join(outcome.command)} could not run: {error}")
    if completed.returncode == 0:
        return CheckResult(True, f"{' '.join(outcome.command)} exited 0")
    tail = (completed.stdout + completed.stderr).strip().splitlines()[-8:]
    return CheckResult(
        False,
        f"{' '.join(outcome.command)} exited {completed.returncode}: " + " | ".join(tail),
    )


def _check_file_contains(outcome: OutcomeSpec, workspace_dir: Path) -> CheckResult:
    if outcome.path is None:
        raise ValueError("a file_contains outcome needs a path")
    target = workspace_dir / outcome.path
    try:
        text = target.read_text()
    except OSError as error:
        return CheckResult(False, f"{outcome.path} could not be read: {error}")
    missing = [needle for needle in outcome.contains if needle not in text]
    present = [needle for needle in outcome.not_contains if needle in text]
    if missing or present:
        detail = []
        if missing:
            detail.append(f"missing {missing!r}")
        if present:
            detail.append(f"still contains {present!r}")
        return CheckResult(False, f"{outcome.path}: " + "; ".join(detail))
    return CheckResult(True, f"{outcome.path} matched every expectation")


def _check_answer_contains(
    outcome: OutcomeSpec,
    report: dict[str, Any] | None,
    *,
    tree_unchanged: bool | None,
) -> CheckResult:
    """Checks the turn's final text — a topology's generic templated answer or
    a conversational turn's own reply, `RunReport.result` either way — for the
    declared substrings, and optionally that nothing on disk moved.
    """
    if report is None:
        return CheckResult(False, "no JSON report was captured")
    if report.get("status") != "completed":
        return CheckResult(False, f"status was {report.get('status')!r}, not completed")
    answer = report.get("result") or ""
    missing = [needle for needle in outcome.answer_contains if needle not in answer]
    if missing:
        return CheckResult(False, f"answer missing {missing!r}: {answer!r}")
    if outcome.tree_unchanged and tree_unchanged is False:
        return CheckResult(False, "the workspace tree changed, but this route must write nothing")
    return CheckResult(True, f"answer: {answer!r}")


def _unversioned(identity: str | None) -> str | None:
    """Strips a trailing ``@<version>`` from a Cosmos versioned-key identity.

    `CompositionRecipeId`/`CompositionBlockId` serialize with their version
    suffixed (``polaris-code.recipe.direct-verified@1.0.0``); `expect.toml`
    names the stable identity a fixture is calibrated against without pinning
    a version that could bump independently of this suite, so the comparison
    ignores everything from the last ``@`` onward.
    """
    if identity is None:
        return None
    return identity.rsplit("@", 1)[0]


def check_plan(plan: PlanSpec, report: dict[str, Any] | None) -> CheckResult:
    """Checks the chosen plan (or its deliberate absence) against ``expect.toml``."""
    topology = (report or {}).get("topology")
    if not plan.expected:
        if topology is not None:
            return CheckResult(
                False,
                f"expected no topology decision, but got {topology.get('registration_id')!r}",
            )
        return CheckResult(True, "no topology decision, as expected: " + plan.rationale_note)

    if topology is None:
        return CheckResult(False, "expected a topology decision, but got none")

    problems: list[str] = []
    if plan.registration_id is not None and topology.get("registration_id") != plan.registration_id:
        problems.append(
            f"registration_id {topology.get('registration_id')!r} != {plan.registration_id!r}"
        )
    composition = topology.get("composition") or {}
    if plan.recipe is not None and _unversioned(composition.get("recipe")) != plan.recipe:
        problems.append(f"recipe {composition.get('recipe')!r} != {plan.recipe!r}")
    actual_blocks = tuple(_unversioned(block) for block in composition.get("blocks") or ())
    if plan.blocks and actual_blocks != plan.blocks:
        problems.append(f"blocks {composition.get('blocks')!r} != {list(plan.blocks)!r}")
    if problems:
        return CheckResult(False, "; ".join(problems))
    return CheckResult(
        True,
        f"{topology.get('registration_id')} / {composition.get('recipe')} / "
        f"{composition.get('blocks')}",
    )
