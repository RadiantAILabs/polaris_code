"""Proves `spec.py`'s ``--topology-task`` strings round-trip through the exact
grammar the binary parses them with.

This package has no FFI into the Rust crate, so it cannot call the real
parser. Instead, ``_reference_split_fields``/``_reference_read_value`` below
are a direct, deliberately literal port of
``crates/polaris_code/src/terminal/topology.rs``'s ``split_fields``/
``read_value`` (as read at the time this suite was written — cite the
functions, not just this file, if that parser ever changes): same
whitespace-skipping, same `key=value` split, same quote and `\\n`/`\\t`/`\\r`/
`\\\\`/escaped-quote handling, char for char. If this port and the real parser
ever disagree, one of them is wrong; keeping the port small and literal is
what makes that disagreement easy to spot by re-reading both side by side.
"""

from __future__ import annotations

import unittest
from pathlib import Path

from .. import expect as expect_mod
from ..spec import build_argv, topology_task_spec

TASKS_DIR = Path(__file__).resolve().parents[3] / "evals" / "tasks"


def _reference_read_value(text: str, index: int) -> tuple[str, int]:
    if index >= len(text) or text[index] not in "\"'":
        start = index
        while index < len(text) and not text[index].isspace():
            index += 1
        return text[start:index], index
    quote = text[index]
    index += 1
    value: list[str] = []
    while True:
        if index >= len(text):
            raise ValueError(f"unterminated {quote!r} quote")
        char = text[index]
        if char == quote:
            return "".join(value), index + 1
        if char == "\\":
            index += 1
            if index >= len(text):
                raise ValueError("dangling backslash")
            escape = text[index]
            if escape == "n":
                value.append("\n")
            elif escape == "t":
                value.append("\t")
            elif escape == "r":
                value.append("\r")
            elif escape == "\\":
                value.append("\\")
            elif escape == quote:
                value.append(escape)
            else:
                raise ValueError(f"unknown escape \\{escape}")
            index += 1
        else:
            value.append(char)
            index += 1


def _reference_split_fields(spec: str) -> list[tuple[str, str]]:
    fields: list[tuple[str, str]] = []
    index = 0
    length = len(spec)
    while True:
        while index < length and spec[index].isspace():
            index += 1
        if index >= length:
            break
        key_start = index
        while True:
            if index >= length:
                raise ValueError("field has no =value")
            char = spec[index]
            if char == "=":
                break
            if char.isspace():
                raise ValueError("field has no =value")
            index += 1
        key = spec[key_start:index]
        index += 1  # skip '='
        value, index = _reference_read_value(spec, index)
        fields.append((key.lower(), value))
    return fields


class SpecRoundTripTest(unittest.TestCase):
    def _fields(self, name: str) -> dict[str, str]:
        loaded = expect_mod.load(TASKS_DIR / name)
        spec = topology_task_spec(loaded.route, default_ref=loaded.name)
        return dict(_reference_split_fields(spec))

    def test_topology_edit_round_trips_find_and_replace_with_real_newlines(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "py-oneline-fix")
        fields = self._fields("py-oneline-fix")
        self.assertEqual(fields["file"], loaded.route.file)
        self.assertEqual(fields["find"], loaded.route.find)
        self.assertEqual(fields["replace"], loaded.route.replace)
        self.assertEqual(fields["intent"], "fast")
        self.assertEqual(fields["budget-ms"], "1000")
        self.assertIn("\n", fields["find"], "the fixture's find text spans two lines")

    def test_careful_edit_has_no_budget_field(self) -> None:
        spec = topology_task_spec(
            expect_mod.load(TASKS_DIR / "rs-careful-verify-edit").route,
            default_ref="rs-careful-verify-edit",
        )
        self.assertNotIn("budget-ms=", spec)

    def test_topology_statement_round_trips_and_carries_intent(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-read-only-question")
        fields = self._fields("rs-read-only-question")
        self.assertEqual(fields["statement"], loaded.route.statement)
        self.assertEqual(fields["intent"], "answer")
        self.assertNotIn("topology", fields)

    def test_directed_route_carries_the_topology_field(self) -> None:
        # No shipped fixture directs a topology (see
        # evals/tasks/rs-survey-coverage-audit/task.md); this constructs the
        # route by hand to prove the field round-trips.
        from ..expect import RouteSpec

        route = RouteSpec(kind="topology-statement", statement="s", topology="code.coverage-audit")
        fields = dict(_reference_split_fields(topology_task_spec(route, default_ref="ref")))
        self.assertEqual(fields["topology"], "code.coverage-audit")

    def test_a_value_containing_a_quote_and_backslash_round_trips(self) -> None:
        from ..expect import RouteSpec

        route = RouteSpec(kind="topology-edit", file="a.rs", find='say "hi"\\bye', replace="x")
        spec = topology_task_spec(route, default_ref="ref")
        fields = dict(_reference_split_fields(spec))
        self.assertEqual(fields["find"], 'say "hi"\\bye')


class ArgvBuildingTest(unittest.TestCase):
    def test_topology_route_uses_topology_task_flag(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "py-oneline-fix")
        argv = build_argv(loaded.route, default_ref=loaded.name)
        self.assertIn("--topology-task", argv)
        self.assertIn("--allow-edit", argv)
        self.assertNotIn("--print", argv)

    def test_conversational_route_uses_print_flag_and_allow_command(self) -> None:
        loaded = expect_mod.load(TASKS_DIR / "rs-repair-loop")
        argv = build_argv(loaded.route, default_ref=loaded.name)
        self.assertIn("--print", argv)
        self.assertIn(loaded.route.prompt, argv)
        self.assertIn("--allow-command", argv)
        self.assertIn("cargo *", argv)
        self.assertNotIn("--topology-task", argv)


if __name__ == "__main__":
    unittest.main()
