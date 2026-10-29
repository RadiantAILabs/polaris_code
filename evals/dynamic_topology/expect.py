"""Loads one fixture's ``expect.toml`` into typed data.

The schema is this package's own — nothing upstream defines an ``expect.toml``
format — and it is deliberately small: a ``[route]`` table saying how the
fixture's task reaches the binary (one of the two headless topology
affordances, or the plain conversational turn), an ``[outcome]`` table saying
how to tell afterwards whether the work landed, and a ``[plan]`` table saying
what the chosen plan is expected to look like, or stating plainly that no plan
is expected and why.
"""

from __future__ import annotations

import dataclasses
import tomllib
from pathlib import Path
from typing import Any


class ExpectError(ValueError):
    """``expect.toml`` is missing a field or names one this schema rejects."""


#: Route kinds this schema accepts. ``topology-edit`` and ``topology-statement``
#: submit through ``--topology-task``; ``conversational`` submits through
#: ``--print`` and runs the full agent loop, which needs a real model.
ROUTE_KINDS = ("topology-edit", "topology-statement", "conversational")

#: ``check`` values ``[outcome]`` accepts.
OUTCOME_CHECKS = ("command", "file_contains", "answer_contains")


@dataclasses.dataclass(frozen=True)
class RouteSpec:
    """How this fixture's task is submitted to the binary."""

    kind: str
    #: ``topology-edit`` only.
    file: str | None = None
    find: str | None = None
    replace: str | None = None
    #: ``topology-statement`` only.
    statement: str | None = None
    #: ``topology-edit`` and ``topology-statement``.
    intent: str | None = None
    budget_ms: int | None = None
    topology: str | None = None
    ref: str | None = None
    #: ``conversational`` only.
    prompt: str | None = None
    requires_live_model: bool = False
    #: Common to every kind.
    allow_edit: bool = True
    allow_command: tuple[str, ...] = ()


@dataclasses.dataclass(frozen=True)
class OutcomeSpec:
    """How to tell, afterwards, whether the fixture's work landed."""

    check: str
    #: ``command``.
    command: tuple[str, ...] = ()
    cwd: str = "."
    #: ``file_contains``.
    path: str | None = None
    contains: tuple[str, ...] = ()
    not_contains: tuple[str, ...] = ()
    #: ``answer_contains``.
    answer_contains: tuple[str, ...] = ()
    tree_unchanged: bool = False


@dataclasses.dataclass(frozen=True)
class PlanSpec:
    """What the chosen plan is expected to look like, per ``topology.*`` JSON."""

    expected: bool
    registration_id: str | None = None
    recipe: str | None = None
    blocks: tuple[str, ...] = ()
    mutating: bool | None = None
    loop_shaped: bool | None = None
    directed: bool = False
    rationale_note: str = ""


@dataclasses.dataclass(frozen=True)
class FixtureExpect:
    """One fixture's whole ``expect.toml``, plus its own directory name."""

    name: str
    shape: str
    language: str
    summary: str
    route: RouteSpec
    outcome: OutcomeSpec
    plan: PlanSpec


def _require(table: dict[str, Any], key: str, where: str) -> Any:
    if key not in table:
        raise ExpectError(f"{where} is missing required field `{key}`")
    return table[key]


def _route(table: dict[str, Any], where: str) -> RouteSpec:
    kind = _require(table, "kind", where)
    if kind not in ROUTE_KINDS:
        raise ExpectError(f"{where}.kind `{kind}` is not one of {ROUTE_KINDS}")
    return RouteSpec(
        kind=kind,
        file=table.get("file"),
        find=table.get("find"),
        replace=table.get("replace"),
        statement=table.get("statement"),
        intent=table.get("intent"),
        budget_ms=table.get("budget_ms"),
        topology=table.get("topology"),
        ref=table.get("ref"),
        prompt=table.get("prompt"),
        requires_live_model=bool(table.get("requires_live_model", False)),
        allow_edit=bool(table.get("allow_edit", True)),
        allow_command=tuple(table.get("allow_command", ())),
    )


def _outcome(table: dict[str, Any], where: str) -> OutcomeSpec:
    check = _require(table, "check", where)
    if check not in OUTCOME_CHECKS:
        raise ExpectError(f"{where}.check `{check}` is not one of {OUTCOME_CHECKS}")
    return OutcomeSpec(
        check=check,
        command=tuple(table.get("command", ())),
        cwd=table.get("cwd", "."),
        path=table.get("path"),
        contains=tuple(table.get("contains", ())),
        not_contains=tuple(table.get("not_contains", ())),
        answer_contains=tuple(table.get("answer_contains", ())),
        tree_unchanged=bool(table.get("tree_unchanged", False)),
    )


def _plan(table: dict[str, Any], where: str) -> PlanSpec:
    expected = bool(_require(table, "expected", where))
    return PlanSpec(
        expected=expected,
        registration_id=table.get("registration_id"),
        recipe=table.get("recipe"),
        blocks=tuple(table.get("blocks", ())),
        mutating=table.get("mutating"),
        loop_shaped=table.get("loop_shaped"),
        directed=bool(table.get("directed", False)),
        rationale_note=table.get("rationale_note", ""),
    )


def load(fixture_dir: Path) -> FixtureExpect:
    """Reads and validates ``<fixture_dir>/expect.toml``.

    Raises:
        ExpectError: a required field is missing or a value names something
            this schema does not accept. Raised eagerly, at load time, so a
            typo in a fixture fails the run that reads it rather than an
            assertion three steps later with no context.
    """
    path = fixture_dir / "expect.toml"
    with path.open("rb") as handle:
        table = tomllib.load(handle)
    where = str(path)
    return FixtureExpect(
        name=fixture_dir.name,
        shape=_require(table, "shape", where),
        language=_require(table, "language", where),
        summary=_require(table, "summary", where),
        route=_route(_require(table, "route", where), f"{where} [route]"),
        outcome=_outcome(_require(table, "outcome", where), f"{where} [outcome]"),
        plan=_plan(_require(table, "plan", where), f"{where} [plan]"),
    )


def discover(tasks_dir: Path) -> list[Path]:
    """Returns every fixture directory under ``tasks_dir``, in name order.

    A fixture directory is one that has its own ``expect.toml`` — this is the
    one thing that distinguishes a fixture from an incidental file such as
    this package's own tests fixtures living alongside it.
    """
    return sorted(
        (child for child in tasks_dir.iterdir() if (child / "expect.toml").is_file()),
        key=lambda path: path.name,
    )
