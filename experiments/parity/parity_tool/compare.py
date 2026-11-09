"""Compare two recorded runs of the same scripted session."""

from __future__ import annotations

import difflib
from dataclasses import dataclass, field

from . import extract
from .extract import Appearance, RequestView
from .runner import RunResult
from .session import RunSpec

#: Longest text shown inline for one value.
SHOW = 240

AGREE = "agree"
DIFFER = "differ"
INCOMPLETE = "incomplete"


@dataclass
class Section:
    """One part of a comparison: whether it agrees and what to say about it."""

    name: str
    agrees: bool
    lines: list[str] = field(default_factory=list)


@dataclass
class Comparison:
    """The result for one run of a session."""

    spec: RunSpec
    labels: tuple[str, str]
    status: str
    sections: list[Section]
    #: Differences in the stock agent's own text, as information only.
    stock: Section
    notes: list[str]
    #: What each side saw, for the JSON report.
    facts: dict


def clip(value, limit: int = SHOW) -> str:
    """A value as one short line."""
    text = value if isinstance(value, str) else repr(value)
    text = text.replace("\n", "\\n")
    return text if len(text) <= limit else text[: limit - 1] + "…"


def text_diff(a: str, b: str, labels: tuple[str, str], limit: int = 24) -> list[str]:
    """A short unified diff of two texts, cut to ``limit`` lines."""
    lines = list(
        difflib.unified_diff(
            a.splitlines(), b.splitlines(), fromfile=labels[0], tofile=labels[1], lineterm="", n=1
        )
    )
    if len(lines) > limit:
        lines = lines[:limit] + [f"… ({len(lines) - limit} more diff lines)"]
    return lines


# -- model calls ----------------------------------------------------------------


def _describe_aside(view: RequestView) -> str:
    first = next((text for *_, text in view.stock), "")
    return f"request outside the agent loop (no tool definitions): {clip(first, 100)}"


def compare_calls(
    labels: tuple[str, str],
    mains: tuple[list[RequestView], list[RequestView]],
    asides: tuple[list[RequestView], list[RequestView]],
    runs: tuple[RunResult, RunResult],
) -> Section:
    """Compare how many model calls each build made, and say which are extra."""
    lines: list[str] = []
    agrees = True
    counts = [len(m) for m in mains]
    if counts[0] != counts[1]:
        agrees = False
        more, fewer = (0, 1) if counts[0] > counts[1] else (1, 0)
        lines.append(
            f"{labels[more]} made {counts[more]} agent model calls and {labels[fewer]} made "
            f"{counts[fewer]}. The script gives the same replies in the same order, so the "
            f"runs are not step for step from request {counts[fewer] + 1} on"
            f" (or earlier, if the extra call came sooner)."
        )
    else:
        lines.append(f"both builds made {counts[0]} agent model calls")
    for side in (0, 1):
        for view in asides[side]:
            agrees = False
            lines.append(f"{labels[side]} also made a {_describe_aside(view)}")
    for side in (0, 1):
        if runs[side].beyond_script:
            agrees = False
            lines.append(
                f"{labels[side]} asked for {runs[side].beyond_script} more reply(ies) than the "
                "script holds; the endpoint answered with a fixed message"
            )
    return Section("model calls", agrees, lines)


# -- rule text -------------------------------------------------------------------


def _describe(item: Appearance) -> str:
    where = f"request {item.request}"
    return f"{where}: {item.kind}" + (f" ({item.call_id})" if item.call_id else "")


def compare_rule_text(
    labels: tuple[str, str],
    mains: tuple[list[RequestView], list[RequestView]],
    asides: tuple[list[RequestView], list[RequestView]],
) -> Section:
    """Compare the rule-related text the model was sent, in order, by request."""
    lines: list[str] = []
    lines_a = extract.timeline(mains[0])
    lines_b = extract.timeline(mains[1])
    key = lambda item: (item.request, item.kind, item.text)  # noqa: E731
    matcher = difflib.SequenceMatcher(
        a=[key(i) for i in lines_a], b=[key(i) for i in lines_b], autojunk=False
    )
    agrees = True
    for tag, a0, a1, b0, b1 in matcher.get_opcodes():
        if tag == "equal":
            continue
        agrees = False
        left, right = lines_a[a0:a1], lines_b[b0:b1]
        paired = min(len(left), len(right)) if tag == "replace" else 0
        for x, y in zip(left[:paired], right[:paired]):
            if (x.request, x.kind) == (y.request, y.kind):
                lines.append(f"{_describe(x)} differs:")
                lines.extend("    " + line for line in text_diff(x.text, y.text, labels))
            else:
                lines.append(f"{labels[0]} {_describe(x)} / {labels[1]} {_describe(y)}:")
                lines.extend("    " + line for line in text_diff(x.text, y.text, labels))
        for x in left[paired:]:
            lines.append(f"only {labels[0]} sent {_describe(x)}: {clip(x.text)}")
        for y in right[paired:]:
            lines.append(f"only {labels[1]} sent {_describe(y)}: {clip(y.text)}")
    if agrees:
        # The same items first appeared at the same requests. The conversation
        # is replayed in full each time, so the lists per request should match too.
        for number, (va, vb) in enumerate(zip(mains[0], mains[1]), 1):
            ka = [i.key for i in va.rule_items]
            kb = [i.key for i in vb.rule_items]
            if ka != kb:
                agrees = False
                lines.append(
                    f"request {number} carries different rule text in its replayed history: "
                    f"{len(ka)} item(s) from {labels[0]}, {len(kb)} from {labels[1]}"
                )
    if agrees:
        lines.append(
            f"{len(lines_a)} rule item(s), same text, same order, same requests"
            + (": " + ", ".join(f"{i.kind}@{i.request}" for i in lines_a) if lines_a else "")
        )
    for side in (0, 1):
        found = [item for view in asides[side] for item in view.rule_items]
        if found:
            lines.append(
                f"note: {labels[side]}'s requests outside the agent loop carry {len(found)} rule item(s)"
            )
    return Section("rule text", agrees, lines)


# -- decisions ---------------------------------------------------------------------


def compare_decisions(
    labels: tuple[str, str],
    runs: tuple[RunResult, RunResult],
    mains: tuple[list[RequestView], list[RequestView]],
) -> tuple[Section, tuple[dict, dict]]:
    """Compare what each run decided, and how it ended."""
    facts = []
    for side in (0, 1):
        run = runs[side]
        decided = extract.decisions(run.record, mains[side])
        decided["process exit code"] = run.exit_code
        decided["timed out"] = run.timed_out
        decided["files at the end"] = dict(sorted(run.final_files.items()))
        facts.append(decided)
    lines: list[str] = []
    agrees = True
    for name in dict.fromkeys([*facts[0], *facts[1]]):
        left, right = facts[0].get(name, "<absent>"), facts[1].get(name, "<absent>")
        if left == right:
            continue
        agrees = False
        if name == "files at the end":
            for path in sorted(set(left) | set(right)):
                if left.get(path) != right.get(path):
                    lines.append(
                        f"{name}: {path}: {labels[0]} "
                        f"{'absent' if path not in left else 'present'}, {labels[1]} "
                        f"{'absent' if path not in right else 'present'}"
                        + (" (content differs)" if path in left and path in right else "")
                    )
        elif isinstance(left, list) and isinstance(right, list):
            lines.append(f"{name}:")
            for line in difflib.unified_diff(
                [str(x) for x in left], [str(x) for x in right], lineterm="", n=0
            ):
                if line.startswith(("---", "+++", "@@")):
                    continue
                lines.append(f"    {labels[0] if line[0] == '-' else labels[1]}: {clip(line[1:])}")
        else:
            lines.append(f"{name}: {labels[0]} {clip(left)}; {labels[1]} {clip(right)}")
    if agrees:
        end = facts[0].get("end reason")
        lines.append(f"same decisions; end reason {end}, exit code {facts[0].get('process exit code')}")
    return Section("decisions", agrees, lines), (facts[0], facts[1])


# -- the record -----------------------------------------------------------------------


def compare_records(
    labels: tuple[str, str],
    runs: tuple[RunResult, RunResult],
    ignored: list[str],
) -> Section:
    """Compare the two run records field by field."""
    records = [run.record for run in runs]
    if records[0] is None and records[1] is None:
        return Section("record", True, ["neither build wrote a run record"])
    for side in (0, 1):
        if records[side] is None:
            other = 1 - side
            return Section(
                "record",
                False,
                [f"{labels[side]} wrote no run record; {labels[other]} wrote one with "
                 f"{len(extract.flatten(records[other]))} fields"],
            )
    fields = [
        extract.record_fields(run.record, run.replacements, ignored)  # type: ignore[arg-type]
        for run in runs
    ]
    lines: list[str] = []
    for path in dict.fromkeys([*fields[0], *fields[1]]):
        if path not in fields[1]:
            lines.append(f"{path}: only in {labels[0]} ({clip(fields[0][path])})")
        elif path not in fields[0]:
            lines.append(f"{path}: only in {labels[1]} ({clip(fields[1][path])})")
        elif fields[0][path] != fields[1][path]:
            lines.append(
                f"{path}: {labels[0]} {clip(fields[0][path])}; {labels[1]} {clip(fields[1][path])}"
            )
    agrees = not lines
    if agrees:
        lines.append(f"{len(fields[0])} fields, same values (times and temporary paths left out)")
    return Section("record", agrees, lines)


# -- the stock agent's own text ------------------------------------------------------------


def compare_stock(
    labels: tuple[str, str],
    mains: tuple[list[RequestView], list[RequestView]],
) -> Section:
    """Describe differences in text that is not about the rules. Information only."""
    lines: list[str] = []
    pairs = list(zip(mains[0], mains[1]))
    if not pairs:
        return Section("stock text", True, ["no request pair to compare"])

    def instructions(view: RequestView) -> str:
        return "\n".join(t for where, item, _r, _n, t in view.stock if where == "instructions")

    first_a, first_b = pairs[0]
    if instructions(first_a) == instructions(first_b):
        lines.append("system prompt: same (apart from rule text)")
    else:
        a_text, b_text = instructions(first_a), instructions(first_b)
        lines.append(
            f"system prompt: differs ({len(a_text.splitlines())} lines in {labels[0]}, "
            f"{len(b_text.splitlines())} in {labels[1]})"
        )
        lines.extend("    " + l for l in text_diff(a_text, b_text, labels, limit=40))

    tools_a, tools_b = dict(first_a.tools), dict(first_b.tools)
    only_a, only_b = sorted(set(tools_a) - set(tools_b)), sorted(set(tools_b) - set(tools_a))
    changed = sorted(n for n in set(tools_a) & set(tools_b) if tools_a[n] != tools_b[n])
    if not (only_a or only_b or changed):
        lines.append(f"tool definitions: same ({len(tools_a)} tools)")
    else:
        lines.append(
            f"tool definitions: {len(tools_a)} in {labels[0]}, {len(tools_b)} in {labels[1]}; "
            f"only {labels[0]}: {only_a or 'none'}; only {labels[1]}: {only_b or 'none'}; "
            f"changed: {changed or 'none'}"
        )
    param_diffs = []
    for key in sorted(set(first_a.params) | set(first_b.params)):
        if first_a.params.get(key) != first_b.params.get(key):
            param_diffs.append(
                f"{key}: {labels[0]} {clip(first_a.params.get(key, '<absent>'), 80)}; "
                f"{labels[1]} {clip(first_b.params.get(key, '<absent>'), 80)}"
            )
    lines.append("other request fields: " + ("same" if not param_diffs else "differ"))
    lines.extend("    " + d for d in param_diffs)

    message_diffs = 0
    shown = 0
    for number, (va, vb) in enumerate(pairs, 1):
        sa = [s for s in va.stock if s[0] != "instructions"]
        sb = [s for s in vb.stock if s[0] != "instructions"]
        if sa == sb:
            continue
        message_diffs += 1
        if shown < 6:
            shown += 1
            lines.append(f"request {number}: messages and tool results differ")
            for x, y in zip(sa, sb):
                if x != y:
                    lines.append(f"    {x[1]}/{x[2]} {labels[0]}: {clip(x[4], 140)}")
                    lines.append(f"    {y[1]}/{y[2]} {labels[1]}: {clip(y[4], 140)}")
                    break
            else:
                lines.append(f"    {len(sa)} pieces in {labels[0]}, {len(sb)} in {labels[1]}")
    if message_diffs == 0:
        lines.append("messages and tool results: same")
    elif message_diffs > shown:
        lines.append(f"… and {message_diffs - shown} more request(s) with differing messages")
    differs = (
        instructions(first_a) != instructions(first_b)
        or bool(only_a or only_b or changed or param_diffs or message_diffs)
    )
    return Section("stock text", not differs, lines)


# -- one comparison ------------------------------------------------------------------------


def compare(spec: RunSpec, labels: tuple[str, str], runs: tuple[RunResult, RunResult]) -> Comparison:
    """Compare two runs of ``spec``."""
    views = [extract.main_views(run.requests, run.replacements) for run in runs]
    mains = (views[0][0], views[1][0])
    asides = (views[0][1], views[1][1])
    notes: list[str] = []
    incomplete = False
    for side in (0, 1):
        run = runs[side]
        if run.start_error:
            incomplete = True
            notes.append(f"{labels[side]} could not be started: {run.start_error}")
        elif run.timed_out:
            incomplete = True
            notes.append(f"{labels[side]} did not finish within the time limit and was stopped")
        if run.dropped_options:
            notes.append(
                f"{labels[side]} has no rules options, so these were left out of its command: "
                + " ".join(run.dropped_options)
            )
        if run.exit_code is not None and run.exit_code != 0 and not run.requests:
            notes.append(
                f"{labels[side]} exited with code {run.exit_code} before any model call; stderr: "
                + clip(run.stderr.strip().splitlines()[-1] if run.stderr.strip() else "(empty)", 300)
            )
    sections = [
        compare_calls(labels, mains, asides, runs),
        compare_rule_text(labels, mains, asides),
    ]
    decisions_section, facts = compare_decisions(labels, runs, mains)
    sections.append(decisions_section)
    sections.append(compare_records(labels, runs, extract.IGNORED_RECORD_FIELDS))
    stock = compare_stock(labels, mains)
    if incomplete:
        status = INCOMPLETE
    elif all(section.agrees for section in sections):
        status = AGREE
    else:
        status = DIFFER
    return Comparison(
        spec=spec,
        labels=labels,
        status=status,
        sections=sections,
        stock=stock,
        notes=notes,
        facts={
            "decisions": {labels[0]: facts[0], labels[1]: facts[1]},
            "requests": {labels[0]: len(mains[0]), labels[1]: len(mains[1])},
            "network_attempts": {labels[0]: runs[0].network_attempts, labels[1]: runs[1].network_attempts},
        },
    )
