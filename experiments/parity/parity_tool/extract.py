"""Read what a run sent the model and what it decided.

Everything here works on the recorded requests (Responses request bodies) and
the run record, never on the agent's own code. The rule-related text is found
by its wording, so the same reading applies to any build:

* a section headed ``## Rules for this task`` in the system prompt (``prompt``);
* a tool result that starts ``Refused before it ran`` (``refusal``);
* text that starts ``[Rules check]`` or ``[Recorded check``, either alone or at
  the end of a message or tool result: a ``hold`` when it says the final message
  was not accepted, a ``note`` otherwise;
* any other message or result that names the rules or the give-up marker
  (``other``).

The model's own messages and calls are the script, the same for both builds, so
they are never read for rule text.
"""

from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass

PROMPT_HEADING = re.compile(r"(?m)^## Rules for this task[ \t]*$")
NEXT_HEADING = re.compile(r"(?m)^#{1,2} \S")
NOTICE_START = re.compile(r"(?m)^\[(?:Rules check|Recorded check)")
REFUSAL_START = "Refused before it ran"
HOLD_PHRASE = "was not accepted"
#: Wording that marks a message as being about the rules even without a prefix.
OTHER_MARKERS = [
    "CANNOT_COMPLETE",
    "The rules for this task",
    "This breaks rule",
    "Do not change, delete, rename or skip tests",
    "Rules for this task",
]


# -- normalising --------------------------------------------------------------


#: Names the agent makes up at random for its own temporary folders.
RANDOM_NAMES = [(re.compile(r"polaris-check-[A-Za-z0-9]+"), "polaris-check-<id>")]


def normalize(value, replacements: list[tuple[str, str]]):
    """Replace run-specific strings by stable names, in every string of ``value``."""
    if isinstance(value, str):
        for needle, name in replacements:
            if needle:
                value = value.replace(needle, name)
        for pattern, name in RANDOM_NAMES:
            value = pattern.sub(name, value)
        return value
    if isinstance(value, list):
        return [normalize(item, replacements) for item in value]
    if isinstance(value, dict):
        return {key: normalize(item, replacements) for key, item in value.items()}
    return value


def short_hash(text: str) -> str:
    """A short stable hash for naming a long text."""
    return hashlib.sha256(text.encode()).hexdigest()[:12]


# -- one request --------------------------------------------------------------


@dataclass(frozen=True)
class Segment:
    """One piece of text in a request, with where it sits."""

    where: str  # "instructions" or "input[<n>]"
    item: str  # "instructions", "message", "result", "call"
    role: str  # "system", "user", "assistant", "tool" or ""
    call_id: str
    name: str  # tool name for calls
    text: str


@dataclass(frozen=True)
class RuleItem:
    """A piece of rule-related text the model was sent."""

    kind: str  # prompt, refusal, note, hold, other
    where: str
    call_id: str
    text: str

    @property
    def key(self) -> tuple[str, str]:
        return (self.kind, self.text)


def _text_of(value) -> str:
    """The text of a Responses content value (string, parts, or other JSON)."""
    if value is None:
        return ""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        parts = []
        for part in value:
            if isinstance(part, dict) and isinstance(part.get("text"), str):
                parts.append(part["text"])
            elif isinstance(part, str):
                parts.append(part)
            else:
                parts.append(json.dumps(part, sort_keys=True))
        return "\n".join(parts)
    return json.dumps(value, sort_keys=True)


def segments(body: dict) -> list[Segment]:
    """Every text of a request body, in order: instructions first, then input."""
    found: list[Segment] = []
    if body.get("instructions"):
        found.append(
            Segment("instructions", "instructions", "system", "", "", _text_of(body["instructions"]))
        )
    items = body.get("input")
    if isinstance(items, str):
        items = [{"type": "message", "role": "user", "content": items}]
    for number, item in enumerate(items or []):
        where = f"input[{number}]"
        kind = item.get("type") or "message"
        if kind == "message":
            content = item.get("content")
            role = item.get("role", "")
            if isinstance(content, list) and content:
                for part in content:
                    found.append(Segment(where, "message", role, "", "", _text_of([part])))
            else:
                found.append(Segment(where, "message", role, "", "", _text_of(content)))
        elif kind == "function_call_output":
            found.append(
                Segment(where, "result", "tool", item.get("call_id", ""), "", _text_of(item.get("output")))
            )
        elif kind == "function_call":
            found.append(
                Segment(
                    where,
                    "call",
                    "assistant",
                    item.get("call_id", ""),
                    item.get("name", ""),
                    _text_of(item.get("arguments")),
                )
            )
        elif kind == "reasoning":
            continue
        else:
            found.append(Segment(where, kind, item.get("role", ""), "", "", _text_of(item)))
    return found


def split_rule_text(segment: Segment) -> tuple[str, list[RuleItem]]:
    """Split a segment into its stock text and the rule-related items in it.

    Returns the text left once the rule items are taken out, and the items.
    Model-authored segments (assistant messages and calls) have none.
    """
    if segment.item == "call" or segment.role == "assistant":
        return segment.text, []
    text = segment.text
    items: list[RuleItem] = []

    heading = PROMPT_HEADING.search(text)
    if heading:
        after = NEXT_HEADING.search(text, heading.end())
        end = after.start() if after else len(text)
        section = text[heading.start() : end].rstrip("\n")
        items.append(RuleItem("prompt", segment.where, segment.call_id, section))
        return text[: heading.start()] + text[end:], items

    if text.lstrip().startswith(REFUSAL_START):
        return "", [RuleItem("refusal", segment.where, segment.call_id, text)]

    notice = NOTICE_START.search(text)
    if notice:
        span = text[notice.start() :]
        kind = "hold" if HOLD_PHRASE in span.split("\n", 1)[0] + span[:400] else "note"
        items.append(RuleItem(kind, segment.where, segment.call_id, span))
        return text[: notice.start()], items

    if any(marker in text for marker in OTHER_MARKERS):
        return "", [RuleItem("other", segment.where, segment.call_id, text)]
    return text, []


@dataclass
class RequestView:
    """One recorded request, split into rule text and the rest."""

    number: int  # 1-based position among the agent's own requests
    auxiliary: bool
    rule_items: list[RuleItem]
    #: The stock texts left once rule items are removed: (where, item, role, name, text).
    stock: list[tuple[str, str, str, str, str]]
    #: Tools as (name, hash of description and schema).
    tools: list[tuple[str, str]]
    #: Every other top-level field of the body, as JSON text.
    params: dict[str, str]
    #: Map of call id to (tool name, hash of arguments), from the calls replayed.
    calls: dict[str, tuple[str, str]]


def view_request(request: dict, number: int, replacements: list[tuple[str, str]]) -> RequestView:
    """Read one recorded request."""
    body = normalize(request.get("body") or {}, replacements)
    rule_items: list[RuleItem] = []
    stock: list[tuple[str, str, str, str, str]] = []
    calls: dict[str, tuple[str, str]] = {}
    for segment in segments(body):
        rest, items = split_rule_text(segment)
        rule_items.extend(items)
        if rest.strip():
            stock.append((segment.where, segment.item, segment.role, segment.name, rest))
        if segment.item == "call":
            calls[segment.call_id] = (segment.name, short_hash(segment.text))
    tools = []
    for tool in body.get("tools") or []:
        name = tool.get("name") or (tool.get("function") or {}).get("name") or ""
        tools.append((name, short_hash(json.dumps(tool, sort_keys=True))))
    params = {
        key: json.dumps(value, sort_keys=True)
        for key, value in body.items()
        if key not in ("input", "instructions", "tools")
    }
    return RequestView(
        number=number,
        auxiliary=bool(request.get("auxiliary")),
        rule_items=rule_items,
        stock=stock,
        tools=tools,
        params=params,
        calls=calls,
    )


# -- the timeline of rule text -------------------------------------------------


@dataclass(frozen=True)
class Appearance:
    """Rule text, with the first request that carried it."""

    request: int
    kind: str
    call_id: str
    text: str


def timeline(views: list[RequestView]) -> list[Appearance]:
    """The rule items in the order they first appear, each with its request.

    A request replays the whole conversation, so an item is listed once, at the
    first request that holds it. An item that repeats as often as before is not
    new; a second copy of the same text is.
    """
    seen: dict[tuple[str, str], int] = {}
    found: list[Appearance] = []
    for view in views:
        count: dict[tuple[str, str], int] = {}
        for item in view.rule_items:
            count[item.key] = count.get(item.key, 0) + 1
            if count[item.key] > seen.get(item.key, 0):
                found.append(Appearance(view.number, item.kind, item.call_id, item.text))
        for key, n in count.items():
            seen[key] = max(seen.get(key, 0), n)
    return found


def main_views(requests: list[dict], replacements: list[tuple[str, str]]) -> tuple[list[RequestView], list[RequestView]]:
    """Views of the agent's own requests, and of the ones outside its loop."""
    main: list[RequestView] = []
    aside: list[RequestView] = []
    for request in requests:
        outside = request.get("auxiliary") or request.get("unexpected") or request.get("body") is None
        if outside:
            aside.append(view_request(request, len(aside) + 1, replacements))
        else:
            main.append(view_request(request, len(main) + 1, replacements))
    return main, aside


# -- decisions ------------------------------------------------------------------


def decisions(record: dict | None, views: list[RequestView]) -> dict:
    """What the run decided, in plain facts, from its record and requests."""
    out: dict = {}
    refused_seen = []
    held_seen = []
    last_calls: dict[str, tuple[str, str]] = {}
    for view in views:
        last_calls.update(view.calls)
    for appearance in timeline(views):
        if appearance.kind == "refusal":
            tool = last_calls.get(appearance.call_id, ("?", ""))[0]
            refused_seen.append(f"request {appearance.request}: {tool} ({appearance.call_id})")
        elif appearance.kind == "hold":
            held_seen.append(f"request {appearance.request}")
    out["refusals the model was sent"] = refused_seen
    out["holds the model was sent"] = held_seen
    if record is None:
        return out
    out["refused calls (record)"] = [
        f"{r.get('tool')} rule {r.get('rule')} {r.get('code')} ({r.get('tool_call_id')})"
        for r in record.get("refusals", [])
    ]
    out["files put back (record)"] = [
        f"{t.get('path')} {t.get('kind')} restored={t.get('restored')} after {t.get('after_tool_calls')} calls"
        for t in record.get("tamper_events", [])
    ]
    out["done gate (record)"] = [
        f"{g.get('action')}: {g.get('reason')}" for g in record.get("gate_events", [])
    ]
    out["solution findings (record)"] = [
        f"{s.get('trigger')}: {s.get('path')} {s.get('code')}" for s in record.get("solution_findings", [])
    ]
    out["checks (record)"] = [
        f"{c.get('trigger')}: passed={c.get('passed')} exit={c.get('exit_code')} timed_out={c.get('timed_out')}"
        for c in record.get("checks", [])
    ]
    out["protected at end (record)"] = [
        f"{p.get('path')} {p.get('state')}" for p in record.get("protected_at_end", [])
    ]
    out["tool outcomes (record)"] = [
        f"{t.get('name')}: {t.get('outcome')}" for t in record.get("tool_calls", [])
    ]
    out["end reason"] = record.get("end_reason")
    out["end detail"] = record.get("end_detail")
    out["exit code (record)"] = record.get("exit_code")
    out["give-up reason"] = record.get("give_up_reason")
    out["claims tests pass"] = record.get("claims_tests_pass")
    out["message count"] = record.get("message_count")
    return out


# -- the record -------------------------------------------------------------------

#: Record fields that hold times or depend on them, as dotted paths with ``[*]``
#: for any list position. They are left out of the comparison.
IGNORED_RECORD_FIELDS = [
    "started_unix_ms",
    "ended_unix_ms",
    "model_calls[*].at_ms",
    "checks[*].duration_ms",
]


def _path_matches(path: str, pattern: str) -> bool:
    return re.fullmatch(re.escape(pattern).replace(r"\[\*\]", r"\[\d+\]"), path) is not None


def flatten(value, prefix: str = "") -> dict[str, object]:
    """A JSON value as ``{dotted path: leaf}``. Empty containers are leaves."""
    if isinstance(value, dict) and value:
        out: dict[str, object] = {}
        for key, item in value.items():
            out.update(flatten(item, f"{prefix}.{key}" if prefix else key))
        return out
    if isinstance(value, list) and value:
        out = {}
        for number, item in enumerate(value):
            out.update(flatten(item, f"{prefix}[{number}]"))
        return out
    return {prefix: value}


def _without_path_bound_hashes(record: dict, replacements: list[tuple[str, str]]) -> dict:
    """``record`` without the output hash of a check whose output names a temp folder.

    The hash is taken over the output as the test command printed it, folder
    name included, so it differs between two runs of the same build. The
    check's ``output_tail`` is still compared, with the folder names replaced.
    """
    checks = record.get("checks")
    if not isinstance(checks, list):
        return record
    kept = []
    for check in checks:
        tail = check.get("output_tail") if isinstance(check, dict) else None
        if isinstance(tail, str) and normalize(tail, replacements) != tail:
            check = {key: value for key, value in check.items() if key != "output_sha256"}
        kept.append(check)
    return {**record, "checks": kept}


def record_fields(record: dict, replacements: list[tuple[str, str]], ignored: list[str]) -> dict[str, object]:
    """The record as comparable ``{path: value}``, without times, paths normalised."""
    leaves = flatten(normalize(_without_path_bound_hashes(record, replacements), replacements))
    return {
        path: value
        for path, value in leaves.items()
        if not any(_path_matches(path, pattern) for pattern in ignored)
    }
