"""Scripted sessions: a small workspace and the replies a fake model gives.

A session is a directory::

    sessions/<name>/
        session.json      what to run and what the model replies
        workspace/        the files the agent starts with (unless "workspace" names a shared one)
        ...               any other file the arguments name, e.g. rules.txt
    sessions/_workspaces/<shared>/
                          workspaces several sessions start from

``session.json`` has these fields (all but ``replies`` are optional):

``description``  one sentence on what the session shows.
``workspace``    the name of a folder under ``sessions/_workspaces/`` to start from,
                 instead of the session's own ``workspace/`` folder.
``exercises``    short names of the mechanisms it exercises.
``task``         the prompt given with ``-p``.
``max_turns``    the ``--max-turns`` limit (default 12).
``args``         extra command-line arguments shared by every run.
``env``          extra environment variables shared by every run.
``runs``         a list of ``{"name": ..., "args": [...], "env": {...}}``. Each is
                 one run of the same workspace and replies with its own extra
                 arguments. Without ``runs`` there is one run, named ``default``.
``ignore_files`` glob patterns of workspace files left out of the end-state
                 comparison (hidden folders and bytecode are always left out).
``replies``      the model's replies in order; see below.

A reply is ``{"text": "...", "calls": [{"tool": "read_file", "arguments":
{...}}], "reasoning": "..."}``. Every field is optional; a reply with calls
asks for them, a reply with only text ends the model's turn.

In ``args``, ``env`` and the arguments of ``calls``, ``{workspace}`` is the
run's workspace folder and ``{session}`` is the session folder.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

DEFAULT_TASK = "Make solution.py pass test.py."
DEFAULT_MAX_TURNS = 12

#: Folder under the sessions folder that holds workspaces shared by sessions.
SHARED_WORKSPACES = "_workspaces"


class SessionError(Exception):
    """A session file is missing something or is malformed."""


@dataclass
class RunSpec:
    """One run of a session: the session plus one variant's arguments."""

    session: "Session"
    name: str
    args: list[str]
    env: dict[str, str]

    @property
    def id(self) -> str:
        return f"{self.session.name}/{self.name}"


@dataclass
class Session:
    """A scripted session loaded from its folder."""

    name: str
    path: Path
    description: str
    exercises: list[str]
    task: str
    max_turns: int
    args: list[str]
    env: dict[str, str]
    runs: list[dict]
    ignore_files: list[str]
    replies: list[dict]
    #: The folder copied to make each run's workspace.
    workspace: Path

    def run_specs(self) -> list[RunSpec]:
        """Every run of this session, in file order."""
        variants = self.runs or [{"name": "default"}]
        return [
            RunSpec(
                session=self,
                name=variant["name"],
                args=[*self.args, *variant.get("args", [])],
                env={**self.env, **variant.get("env", {})},
            )
            for variant in variants
        ]


def expand(value, mapping: dict[str, str]):
    """Replace ``{key}`` placeholders in every string inside ``value``."""
    if isinstance(value, str):
        for key, replacement in mapping.items():
            value = value.replace("{" + key + "}", replacement)
        return value
    if isinstance(value, list):
        return [expand(item, mapping) for item in value]
    if isinstance(value, dict):
        return {key: expand(item, mapping) for key, item in value.items()}
    return value


def load_session(path: Path) -> Session:
    """Read one session folder."""
    file = path / "session.json"
    try:
        data = json.loads(file.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise SessionError(f"{file}: {error}") from error
    if not isinstance(data.get("replies"), list) or not data["replies"]:
        raise SessionError(f"{file}: 'replies' must be a non-empty list")
    names = [run.get("name") for run in data.get("runs", [])]
    if any(not isinstance(name, str) or not name for name in names):
        raise SessionError(f"{file}: every run needs a name")
    if len(set(names)) != len(names):
        raise SessionError(f"{file}: run names must be unique")
    shared = data.get("workspace")
    if shared is None:
        workspace = path / "workspace"
    elif isinstance(shared, str) and shared and "/" not in shared and "\\" not in shared:
        workspace = path.parent / SHARED_WORKSPACES / shared
    else:
        raise SessionError(f"{file}: 'workspace' must be the name of a shared workspace")
    if not workspace.is_dir():
        raise SessionError(f"{file}: missing the workspace folder {workspace}")
    return Session(
        name=path.name,
        path=path,
        description=data.get("description", ""),
        exercises=list(data.get("exercises", [])),
        task=data.get("task", DEFAULT_TASK),
        max_turns=int(data.get("max_turns", DEFAULT_MAX_TURNS)),
        args=list(data.get("args", [])),
        env=dict(data.get("env", {})),
        runs=list(data.get("runs", [])),
        ignore_files=list(data.get("ignore_files", [])),
        replies=data["replies"],
        workspace=workspace,
    )


def load_sessions(root: Path) -> list[Session]:
    """Every session folder under ``root``, in name order."""
    if not root.is_dir():
        raise SessionError(f"{root}: not a folder")
    return [
        load_session(path)
        for path in sorted(root.iterdir())
        if (path / "session.json").is_file()
    ]


def select(specs: list[RunSpec], patterns: list[str]) -> list[RunSpec]:
    """Keep runs whose id contains any of ``patterns`` (all when empty)."""
    if not patterns:
        return specs
    return [spec for spec in specs if any(p in spec.id for p in patterns)]
