"""Builds the argv ``polaris-code`` actually runs for one fixture's route.

Grounded in the spec grammar ``terminal::topology::split_fields``/``read_value``
parse (``crates/polaris_code/src/terminal/topology.rs``): a quoted field
value may contain a real embedded newline verbatim (only `\\`, the quote
character itself, and the four `\n`/`\t`/`\r`/`\\` escapes are special), so this
module quotes a value by escaping just backslash and the double quote it wraps
the value in and otherwise passes the text through unchanged — it never needs
to emit a `\n` escape sequence, because a real newline byte already round-trips
through that parser.
"""

from __future__ import annotations

from .expect import RouteSpec


def quote_spec_value(text: str) -> str:
    """Quotes one ``key=value`` field value for a ``--topology-task`` spec."""
    escaped = text.replace("\\", "\\\\").replace('"', '\\"')
    return f'"{escaped}"'


def topology_task_spec(route: RouteSpec, *, default_ref: str) -> str:
    """Builds the ``--topology-task`` spec string for a topology-* route.

    Raises:
        ValueError: ``route.kind`` is not a topology route.
    """
    fields: list[str] = []
    if route.kind == "topology-edit":
        if route.file is None or route.find is None or route.replace is None:
            raise ValueError("a topology-edit route needs file, find and replace")
        fields.append(f"file={quote_spec_value(route.file)}")
        fields.append(f"find={quote_spec_value(route.find)}")
        fields.append(f"replace={quote_spec_value(route.replace)}")
    elif route.kind == "topology-statement":
        if route.statement is None:
            raise ValueError("a topology-statement route needs statement")
        fields.append(f"statement={quote_spec_value(route.statement)}")
    else:
        raise ValueError(f"{route.kind} is not a topology route")

    if route.intent is not None:
        fields.append(f"intent={route.intent}")
    if route.budget_ms is not None:
        fields.append(f"budget-ms={route.budget_ms}")
    if route.topology is not None:
        fields.append(f"topology={route.topology}")
    fields.append(f"ref={quote_spec_value(route.ref or default_ref)}")
    return " ".join(fields)


def build_argv(route: RouteSpec, *, default_ref: str, output_format: str = "json") -> list[str]:
    """The ``polaris-code`` arguments for one fixture's route, minus the binary
    path and working directory (the caller supplies both).
    """
    argv = ["--output-format", output_format]
    if route.kind in ("topology-edit", "topology-statement"):
        argv += ["--topology-task", topology_task_spec(route, default_ref=default_ref)]
    elif route.kind == "conversational":
        if route.prompt is None:
            raise ValueError("a conversational route needs a prompt")
        argv += ["--print", route.prompt]
    else:
        raise ValueError(f"unknown route kind {route.kind!r}")
    if route.allow_edit:
        argv.append("--allow-edit")
    for pattern in route.allow_command:
        argv += ["--allow-command", pattern]
    return argv
