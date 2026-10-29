"""The one seam between this adapter and the operating system.

Every container command, every agent invocation and every grader invocation
goes through a :class:`ProcessRunner`. There are two implementations and the
difference between them is recorded in the manifest, because it is the
difference between evidence and a rehearsal:

* :class:`SubprocessRunner` really executes. It is what a measured sweep uses.
* :class:`ReplayRunner` answers from a recorded script and executes nothing. It
  is what deterministic replay tests use, on a host
  with no container runtime and no provider key.

A replayed run exercises every line of this adapter — the argv it composes, the
stdout it parses, the patches it collects, the grader report it reads and the
summary it derives. Only the kernel call is substituted. That is why
``process_runner`` is a recorded manifest field and why
:mod:`evals.core.collate` refuses to call a replayed run a
measurement.
"""

from __future__ import annotations

import dataclasses
import json
import subprocess
from pathlib import Path
from typing import Any, Mapping, Protocol, Sequence

#: ``process_runner`` value recorded for a really-executed run.
SUBPROCESS = "subprocess"

#: ``process_runner`` value recorded for a replayed run.
REPLAY = "replay"

#: Exit status this adapter reports when a command exceeded its timeout. It is
#: distinct from every exit code ``polaris-code`` itself defines (0/1/2/130) so
#: a timeout can never be misread as a turn result.
TIMEOUT_RETURNCODE = -62


@dataclasses.dataclass(frozen=True)
class Completed:
    """What one command did.

    ``timed_out`` is carried separately from ``returncode`` because a timeout
    is an episode *result* (unresolved) while a non-zero exit may be either a
    result or an infrastructure failure, and the two must stay distinguishable.
    """

    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str
    timed_out: bool = False

    @property
    def ok(self) -> bool:
        """Whether the command succeeded outright."""
        return self.returncode == 0 and not self.timed_out

    def describe(self) -> str:
        """A one-line, secret-free description for an episode record."""
        state = "timed out" if self.timed_out else f"exit {self.returncode}"
        return f"{' '.join(self.argv)} -> {state}"


class ProcessRunner(Protocol):
    """Runs one command and reports what happened.

    An implementation must never raise for a non-zero exit: a failing agent, a
    failing grader and a failing container are all outcomes this adapter
    records rather than exceptions it propagates.
    """

    #: The value recorded in ``manifest.json`` as ``process_runner``.
    kind: str

    def run(
        self,
        argv: Sequence[str],
        *,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        """Run ``argv`` and return its result."""
        ...


class SubprocessRunner:
    """Really executes commands."""

    kind = SUBPROCESS

    def run(
        self,
        argv: Sequence[str],
        *,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        """Run ``argv``, capturing both streams as UTF-8 with replacement.

        Decoding is lossy on purpose: a container's output is arbitrary bytes
        and an undecodable byte must not lose an entire episode's record.
        """
        try:
            finished = subprocess.run(  # noqa: S603 - argv is composed, never shell
                list(argv),
                env=dict(env) if env is not None else None,
                timeout=timeout,
                cwd=str(cwd) if cwd is not None else None,
                capture_output=True,
                check=False,
            )
        except subprocess.TimeoutExpired as expired:
            return Completed(
                argv=tuple(argv),
                returncode=TIMEOUT_RETURNCODE,
                stdout=_decode(expired.stdout),
                stderr=_decode(expired.stderr),
                timed_out=True,
            )
        except FileNotFoundError as err:
            return Completed(
                argv=tuple(argv),
                returncode=127,
                stdout="",
                stderr=f"{err}",
            )
        return Completed(
            argv=tuple(argv),
            returncode=finished.returncode,
            stdout=_decode(finished.stdout),
            stderr=_decode(finished.stderr),
        )


def _decode(raw: bytes | str | None) -> str:
    """Decode captured output without ever failing on a stray byte."""
    if raw is None:
        return ""
    if isinstance(raw, str):
        return raw
    return raw.decode("utf-8", errors="replace")


class ReplayError(RuntimeError):
    """A replay script did not cover a command the adapter issued.

    Raised rather than answered with a default: a silent default would let the
    demonstration diverge from the real path without anyone noticing.
    """


@dataclasses.dataclass(frozen=True)
class ReplayStep:
    """One recorded answer.

    ``match`` is a list of substrings that must all appear in the command line.
    Substring matching, rather than exact argv equality, keeps a script
    readable and stable against the run-specific ids the adapter embeds in
    session names and container names.
    """

    match: tuple[str, ...]
    returncode: int = 0
    stdout: str = ""
    stderr: str = ""
    timed_out: bool = False
    #: Files this step leaves behind, relative to the command's working
    #: directory, as ``(relative path, contents)`` pairs. A real command's
    #: side effects are part of its interface: the official grader answers
    #: through a report *file*, not through stdout, so a script that could not
    #: write one would leave the report-reading path untested and every
    #: replayed cell ungraded.
    writes: tuple[tuple[str, str], ...] = ()

    def matches(self, argv: Sequence[str]) -> bool:
        """Whether this step answers ``argv``."""
        line = " ".join(argv)
        return all(token in line for token in self.match)


class ReplayRunner:
    """Answers commands from a recorded script, executing nothing.

    Steps are tried in order and the first match wins, so a script reads
    top-to-bottom as "the specific cases, then the general ones". Every answered
    command is appended to :attr:`issued`, which the tests assert against — the
    argv this adapter *would have run* is the thing under test.
    """

    kind = REPLAY

    def __init__(self, steps: Sequence[ReplayStep]) -> None:
        self._steps = tuple(steps)
        #: Every command the adapter issued, in order.
        self.issued: list[tuple[str, ...]] = []

    @classmethod
    def from_file(cls, path: Path) -> "ReplayRunner":
        """Load a replay script from a JSON file.

        The file is a list of objects with ``match`` plus any of ``returncode``,
        ``stdout``, ``stderr`` and ``timed_out``.
        """
        raw = json.loads(path.read_text(encoding="utf-8"))
        return cls([_step(entry) for entry in raw])

    def run(
        self,
        argv: Sequence[str],
        *,
        env: Mapping[str, str] | None = None,
        timeout: float | None = None,
        cwd: Path | None = None,
    ) -> Completed:
        """Answer ``argv`` from the script.

        Raises:
            ReplayError: if no step matches. The unmatched command line is
                included so a script can be extended to cover it.
        """
        self.issued.append(tuple(argv))
        for step in self._steps:
            if step.matches(argv):
                base = Path(cwd) if cwd is not None else Path.cwd()
                for relative, contents in step.writes:
                    target = base / relative
                    target.parent.mkdir(parents=True, exist_ok=True)
                    target.write_text(contents, encoding="utf-8")
                return Completed(
                    argv=tuple(argv),
                    returncode=step.returncode,
                    stdout=step.stdout,
                    stderr=step.stderr,
                    timed_out=step.timed_out,
                )
        raise ReplayError(f"no replay step matches: {' '.join(argv)}")


def _step(entry: Mapping[str, Any]) -> ReplayStep:
    """Build one :class:`ReplayStep` from its JSON form."""
    return ReplayStep(
        match=tuple(entry["match"]),
        returncode=int(entry.get("returncode", 0)),
        stdout=str(entry.get("stdout", "")),
        stderr=str(entry.get("stderr", "")),
        timed_out=bool(entry.get("timed_out", False)),
        writes=tuple((str(name), str(body)) for name, body in entry.get("writes", ())),
    )
