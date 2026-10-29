"""One episode: its identity, its one agent invocation, and its record.

The whole of this harness's contact with the shipped agent is
:func:`agent_argv` — a single ``polaris-code -p`` invocation per episode, the
supported non-interactive entry point, run once. There is no second loop, no
retry of a completed turn and no benchmark text: ``problem_statement`` is
handed over byte-for-byte as the dataset stores it.

What is *not* here is the environment. Creating a sandbox, installing the
binary, checking the tree and collecting a patch are a dataset's business,
because the image, the working directory and the patch convention are the
dataset's; a pack supplies an
:class:`~evals.core.pack.EnvironmentProvider` and calls into this module for
the agent command, the identity and the record.

``--print`` is fail-closed (``docs/permission-modes.md``): an unattended run
denies ``edit_file`` unless it is granted, so without ``--allow-edit`` every
episode would submit an empty patch and every cell would score zero for a
reason that has nothing to do with graph search. The grant is therefore part
of the recorded configuration (``allow_edit``, ``allow_command``) and lands in
the manifest; it uses only flags the shipped binary already has, and adds
nothing to the prompt.
"""

from __future__ import annotations

import dataclasses
import hashlib
import tempfile
from pathlib import Path
from typing import Mapping, Sequence

from .config import (
    API_KEY_VAR,
    ARM_VAR,
    COMPOSITION_POLICY_VAR,
    CONTAINER_BINARY_PATH,
    DOMAIN_VAR,
    GATE_PLANNER_VAR,
    MAX_OUTPUT_TOKENS_VAR,
    MAX_TURNS_VAR,
    GRAPH_SEARCH_VAR,
    MODEL_VAR,
    TOPOLOGY_VAR,
)
from .usage import RunUsage

#: Termination reasons an episode record may carry. The first four are results
#: of the experiment; the last is an infrastructure failure, which is the only
#: one that is ever retried.
TERMINATION_COMPLETED = "completed"
TERMINATION_AGENT_FAILED = "agent-failed"
TERMINATION_TIMEOUT = "timeout"
TERMINATION_UNREADABLE_RESULT = "unreadable-result"
TERMINATION_INFRASTRUCTURE = "infrastructure-failure"

#: Terminations that are experiment results rather than infrastructure faults.
RESULT_TERMINATIONS = (
    TERMINATION_COMPLETED,
    TERMINATION_AGENT_FAILED,
    TERMINATION_TIMEOUT,
    TERMINATION_UNREADABLE_RESULT,
)


def session_id(eval_run_id: str, mode: str, repetition: int, instance_id: str) -> str:
    """The session identity for one cell of the matrix.

    Every component is included so that two episodes can never share a session:
    a shared session would carry one mode's conversation into another's and
    quietly destroy the comparison.
    """
    return f"{eval_run_id}-{mode}-{repetition}-{instance_id}"


def container_name(eval_run_id: str, mode: str, repetition: int, instance_id: str) -> str:
    """A sandbox name unique to one episode, safe for a container runtime."""
    raw = f"polaris-eval-{eval_run_id}-{mode}-r{repetition}-{instance_id}"
    return "".join(c if (c.isalnum() or c in "._-") else "-" for c in raw)[:120]


def agent_argv(
    *,
    binary: str = CONTAINER_BINARY_PATH,
    working_dir: str,
    problem_statement: str,
    session: str,
    allow_edit: bool,
    allow_command: Sequence[str] = (),
    topology_task: str | None = None,
) -> tuple[str, ...]:
    """The one agent invocation, as argv rather than a shell string.

    Composed as argv on purpose: ``problem_statement`` is arbitrary issue text
    from a public dataset and must reach the agent unchanged and unquoted by
    any shell.

    ``working_dir`` is supplied by the pack's environment provider, because
    where a dataset's checkout sits inside its own image is the dataset's
    convention, not the harness's.
    """
    argv = [binary, working_dir]
    if topology_task is None:
        argv.extend(["-p", problem_statement])
    else:
        argv.extend(["--topology-task", topology_task])
    argv.extend([
        "--output-format",
        "json",
        "--session",
        session,
    ])
    if allow_edit:
        argv.append("--allow-edit")
    for glob in allow_command:
        argv.extend(["--allow-command", glob])
    return tuple(argv)


def task_field_quote(text: str) -> str:
    """Quote a value for the binary's ``--topology-task`` field grammar.

    That grammar (``terminal/topology.rs``) accepts any raw character inside
    double quotes and only the escapes ``\\n``, ``\\t``, ``\\r``, ``\\\\`` and
    ``\\"``. ``json.dumps`` is not that grammar: it writes non-ASCII text and
    control characters as ``\\uXXXX`` (or ``\\b``/``\\f``), which the binary
    refuses, so an issue quoting a single accented name never reached the
    agent.
    """
    escaped = (
        text.replace("\\", "\\\\")
        .replace('"', '\\"')
        .replace("\n", "\\n")
        .replace("\t", "\\t")
        .replace("\r", "\\r")
    )
    return f'"{escaped}"'


def statement_task_spec(
    problem_statement: str,
    reference: str,
    *,
    topology: str | None = "code.tree-question",
    intent: str = "answer",
) -> str:
    """Render a general coding task for the binary's existing topology affordance.

    The issue text remains an argv value, not shell text. The default
    ``answer`` intent makes the legacy topology-path treatment use the
    non-mutating tree-question topology. A topology- or composition-axis
    episode omits ``topology`` so the binary leaves selection automatic —
    reading its recorded operator pin for the former, or picking a
    registration on its own for the latter, where only the composition policy
    is meant to vary.
    """
    spec = f"statement={task_field_quote(problem_statement)} ref={reference} intent={intent}"
    return f"{spec} topology={topology}" if topology is not None else spec


def topology_task_spec(
    problem_statement: str,
    reference: str,
    *,
    topology_work: bool,
    topology_axis: bool,
    composition_axis: bool,
    intent: str,
) -> str | None:
    """Build a pack-neutral topology-task argument for one episode.

    Both SWE-bench packs use the same binary affordance. Dataset providers
    retain ownership of their container lifecycle, while this shared helper
    keeps the treatment's topology selection and task profile identical.
    """
    if not topology_work:
        return None
    return statement_task_spec(
        problem_statement,
        reference,
        topology=None if (topology_axis or composition_axis) else "code.tree-question",
        intent=intent,
    )


def agent_env(
    model: str,
    mode: str,
    api_key: str | None,
    *,
    topology_pin: str | None = None,
    composition_policy: str = "deterministic",
    max_turns: int | None = None,
    max_output_tokens: int | None = None,
    domain: str = "none",
    arm: str = "generic",
    extra_env: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """The environment one episode receives.

    Exactly the declared names, matching
    :data:`evals.core.config.ALLOWED_ENV_NAMES`, plus whatever the caller
    passes as `extra_env` — the lean-switch/`MAX_COST_VAR` passthrough
    (:func:`evals.core.config.lean_passthrough_names`) is the one caller that
    does. The key is omitted entirely when absent rather than passed empty,
    so a missing key produces the binary's own exit-2 configuration error
    instead of an authentication failure halfway through a turn.
    """
    env = {
        MODEL_VAR: model,
        GRAPH_SEARCH_VAR: mode,
    }
    if composition_policy == "gates":
        env[COMPOSITION_POLICY_VAR] = "gates"
        env[GATE_PLANNER_VAR] = "scripted"
    elif composition_policy == "gates-model":
        env[COMPOSITION_POLICY_VAR] = "gates"
        env[GATE_PLANNER_VAR] = "model"
    else:
        env[COMPOSITION_POLICY_VAR] = composition_policy
    if topology_pin is not None:
        env[TOPOLOGY_VAR] = topology_pin
    if max_turns is not None:
        env[MAX_TURNS_VAR] = str(max_turns)
    if max_output_tokens is not None:
        env[MAX_OUTPUT_TOKENS_VAR] = str(max_output_tokens)
    # Always set, like MODEL_VAR/GRAPH_SEARCH_VAR above: an explicit "none"/
    # "generic" is not the same as leaving the binary's own default
    # unstated, and a run that never touches domain plans should still say so
    # rather than rely on the two happening to agree.
    env[DOMAIN_VAR] = domain
    env[ARM_VAR] = arm
    if api_key:
        env[API_KEY_VAR] = api_key
    if extra_env:
        env.update(extra_env)
    return env


def axis_agent_env(
    model: str,
    mode: str,
    api_key: str | None,
    *,
    topology_axis: bool,
    composition_axis: bool,
    graph_search_mode: str | None,
    composition_policy: str,
    max_turns: int | None = None,
    max_output_tokens: int | None = None,
    domain: str = "none",
    arm: str = "generic",
    extra_env: Mapping[str, str] | None = None,
) -> dict[str, str]:
    """Build the shared environment for a graph, topology, or composition cell."""
    graph_mode = graph_search_mode if (topology_axis or composition_axis) else mode
    return agent_env(
        model,
        graph_mode or mode,
        api_key,
        topology_pin=mode if topology_axis else None,
        composition_policy=mode if composition_axis else composition_policy,
        max_turns=max_turns,
        max_output_tokens=max_output_tokens,
        domain=domain,
        arm=arm,
        extra_env=extra_env,
    )


@dataclasses.dataclass(frozen=True)
class EpisodeOutcome:
    """Everything one attempted episode produced.

    ``patch`` is the empty string for every unresolved-by-construction case —
    a failed turn, a timeout, an unreadable result — because a benchmark
    treats an empty prediction as unresolved, which is the correct disposition.

    Five fields describe the *environment* rather than the experiment, and are
    filled in by whichever provider the pack supplied: ``image``,
    ``image_digest``, ``base_commit``, ``head_commit`` and ``tree_was_clean``.
    They are ``None``/``False`` for a provider that has no such notion. They
    live on the shared record because a run directory has one episode schema,
    and a reader must be able to check that an episode started from the state
    the suite pinned.
    """

    instance_id: str
    mode: str
    repetition: int
    attempt: int
    termination: str
    exit_code: int
    session_id: str
    turn_number: int | None
    run_id: str | None
    status: str | None
    error: str | None
    usage: RunUsage | None
    #: The output token limit the agent ran under, as the run reported it.
    #: ``None`` when no result was readable, or when the agent predates the
    #: field. A benchmark number is only interpretable alongside this: a
    #: response that reached the limit came back truncated.
    max_output_tokens: int | None
    patch: str
    patch_sha256: str
    patch_bytes: int
    image: str
    image_digest: str | None
    base_commit: str
    head_commit: str | None
    tree_was_clean: bool
    inference_seconds: float
    commands: tuple[str, ...]
    #: The topology runtime's terminal answer, or ``None`` for a
    #: conversational invocation.
    topology_terminal: str | None
    #: The composition policy that assembled this turn's recipe, read back
    #: from the run's own report rather than assumed from what the episode
    #: requested. ``None`` when the turn started no runtime-assembled topology.
    composition_policy: str | None
    #: Versioned identity of the recipe the policy selected, e.g.
    #: ``"direct-verified@v1"``. ``None`` alongside ``composition_policy``.
    composition_recipe: str | None
    #: Stable digest of the whole bounded composition plan
    #: (``"sha256:…"``), the plan's canonical replay identity. ``None``
    #: alongside ``composition_policy``.
    composition_digest: str | None
    #: Whether an explicit caller direction forced this choice rather than
    #: the policy choosing it freely. This harness never issues such a
    #: direction, so a sweep expects this to read ``False`` on every episode
    #: that carries a composition at all; ``None`` alongside
    #: ``composition_policy``.
    composition_forced: bool | None
    #: Canonical identity of the admitted gate-expression plan.
    gate_plan_digest: str | None
    #: Configuration-independent identity of the admitted gate-plan shape.
    gate_plan_shape_digest: str | None
    #: Short human-readable rendering of the admitted gate-plan shape.
    gate_plan_shape: str | None
    #: Planner identity that assembled the admitted gate plan.
    gate_plan_planner: str | None
    #: One-based gate-plan revision attempt that admitted.
    gate_plan_revision_attempt: int | None
    #: Gate-plan attempts refused before the admitted revision.
    gate_plan_prior_refused_attempts: int | None
    #: Refused gate-plan attempts before finite-recipe fallback.
    exhausted_gate_plan_refused_attempts: int | None
    #: The domain this episode requested (see
    #: :data:`evals.core.config.DOMAINS`). ``"none"`` for a pack or run that
    #: does not use domain plans at all — the same default
    #: :class:`~evals.core.config.RunConfig` carries.
    domain: str = "none"
    #: The arm this episode requested (see :data:`evals.core.config.ARMS`).
    arm: str = "generic"
    #: Digest of the notes text `arm` read for `domain`, computed from the
    #: checked-in `notes.md` this episode's binary was built from — `None`
    #: for `domain="none"` or `arm="generic"`, which read no domain notes.
    notes_digest: str | None = None
    #: Transcript files kept for this attempt, as ``(relative name, text)``
    #: pairs, when the run asked for them (``--keep-transcripts``). Written
    #: under the episode's ``logs/`` directory by the runner and never part of
    #: :meth:`to_json`: they carry the issue text and raw model output, which
    #: an ``episodes.jsonl`` record must not.
    transcript: tuple[tuple[str, str], ...] = ()

    @property
    def is_infrastructure_failure(self) -> bool:
        """Whether this attempt failed for a reason outside the experiment."""
        return self.termination == TERMINATION_INFRASTRUCTURE

    @property
    def agent_was_launched(self) -> bool:
        """Whether the agent process was actually started for this attempt.

        A provider detects every infrastructure failure *before* the one agent
        invocation, so an infrastructure failure is exactly the case where no
        provider call was reachable and the episode's API cost is a known zero.
        The cost guard needs that distinction because ``usage is None`` alone
        cannot express it.
        """
        return not self.is_infrastructure_failure

    def to_json(self, schema: str) -> dict[str, object]:
        """The ``episodes.jsonl`` projection.

        Deliberately excludes the problem statement, the assistant's answer and
        any raw model output: the harness stores nothing beyond
        the public dataset field, and the prompt is already pinned in the
        pack's own suite file.
        """
        return {
            "schema": schema,
            "instance_id": self.instance_id,
            "mode": self.mode,
            "repetition": self.repetition,
            "attempt": self.attempt,
            "termination": self.termination,
            "exit_code": self.exit_code,
            "session_id": self.session_id,
            "turn_number": self.turn_number,
            "num_turns": None if self.usage is None else self.usage.num_turns,
            "run_id": self.run_id,
            "status": self.status,
            "error": self.error,
            "usage": None if self.usage is None else self.usage.to_json(),
            "max_output_tokens": self.max_output_tokens,
            "patch_sha256": self.patch_sha256,
            "patch_bytes": self.patch_bytes,
            "image": self.image,
            "image_digest": self.image_digest,
            "base_commit": self.base_commit,
            "head_commit": self.head_commit,
            "tree_was_clean": self.tree_was_clean,
            "inference_seconds": round(self.inference_seconds, 3),
            "commands": list(self.commands),
            "topology_terminal": self.topology_terminal,
            "composition_policy": self.composition_policy,
            "composition_recipe": self.composition_recipe,
            "composition_digest": self.composition_digest,
            "composition_forced": self.composition_forced,
            "gate_plan_digest": self.gate_plan_digest,
            "gate_plan_shape_digest": self.gate_plan_shape_digest,
            "gate_plan_shape": self.gate_plan_shape,
            "gate_plan_planner": self.gate_plan_planner,
            "gate_plan_revision_attempt": self.gate_plan_revision_attempt,
            "gate_plan_prior_refused_attempts": self.gate_plan_prior_refused_attempts,
            "exhausted_gate_plan_refused_attempts": self.exhausted_gate_plan_refused_attempts,
            "domain": self.domain,
            "arm": self.arm,
            "notes_digest": self.notes_digest,
        }


def outcome(
    *,
    instance_id: str,
    mode: str,
    repetition: int,
    attempt: int,
    session: str,
    termination: str,
    exit_code: int,
    turn_number: int | None,
    run_id: str | None,
    status: str | None,
    usage: RunUsage | None,
    max_output_tokens: int | None,
    patch: str,
    image: str,
    image_digest: str | None,
    base_commit: str,
    head_commit: str | None,
    tree_was_clean: bool,
    error: str | None,
    seconds: float,
    commands: Sequence[str],
    topology_terminal: str | None = None,
    composition_policy: str | None = None,
    composition_recipe: str | None = None,
    composition_digest: str | None = None,
    composition_forced: bool | None = None,
    gate_plan_digest: str | None = None,
    gate_plan_shape_digest: str | None = None,
    gate_plan_shape: str | None = None,
    gate_plan_planner: str | None = None,
    gate_plan_revision_attempt: int | None = None,
    gate_plan_prior_refused_attempts: int | None = None,
    exhausted_gate_plan_refused_attempts: int | None = None,
    domain: str = "none",
    arm: str = "generic",
    notes_digest: str | None = None,
    transcript: Sequence[tuple[str, str]] = (),
) -> EpisodeOutcome:
    """Assemble one attempt's record, digesting the patch once.

    Shared rather than left to each provider so that two packs cannot disagree
    about how a prediction is hashed or measured.
    """
    encoded = patch.encode("utf-8")
    return EpisodeOutcome(
        instance_id=instance_id,
        mode=mode,
        repetition=repetition,
        attempt=attempt,
        termination=termination,
        exit_code=exit_code,
        session_id=session,
        turn_number=turn_number,
        run_id=run_id,
        status=status,
        error=error,
        usage=usage,
        max_output_tokens=max_output_tokens,
        patch=patch,
        patch_sha256=hashlib.sha256(encoded).hexdigest(),
        patch_bytes=len(encoded),
        image=image,
        image_digest=image_digest,
        base_commit=base_commit,
        head_commit=head_commit,
        tree_was_clean=tree_was_clean,
        inference_seconds=seconds,
        commands=tuple(commands),
        topology_terminal=topology_terminal,
        composition_policy=composition_policy,
        composition_recipe=composition_recipe,
        composition_digest=composition_digest,
        composition_forced=composition_forced,
        gate_plan_digest=gate_plan_digest,
        gate_plan_shape_digest=gate_plan_shape_digest,
        gate_plan_shape=gate_plan_shape,
        gate_plan_planner=gate_plan_planner,
        gate_plan_revision_attempt=gate_plan_revision_attempt,
        gate_plan_prior_refused_attempts=gate_plan_prior_refused_attempts,
        exhausted_gate_plan_refused_attempts=exhausted_gate_plan_refused_attempts,
        domain=domain,
        arm=arm,
        notes_digest=notes_digest,
        transcript=tuple(transcript),
    )


#: Where ``polaris-code`` keeps its session files, relative to the working
#: directory it runs in (``STATE_DIR_NAME`` in
#: ``crates/polaris_code_core/src/state.rs``). The directory ignores itself,
#: so nothing in it reaches a prediction patch.
AGENT_STATE_DIR = ".polaris"

#: What a kept transcript shows instead of the provider key.
REDACTED_KEY = "[REDACTED:ANTHROPIC_API_KEY]"


def collect_transcript(
    exec_command,
    *,
    runtime: str,
    container: str,
    workdir: str,
    turn,
    api_key: str | None,
) -> tuple[tuple[str, str], ...]:
    """Copy one attempt's transcript out of its container before removal.

    Keeps the agent's stdout (its JSON result, partial on a timeout), its
    stderr, and every file of the session store the agent saved under
    :data:`AGENT_STATE_DIR`. Best effort: a copy that fails leaves the two
    streams, never an exception. The provider key is replaced in every file,
    whatever put it there.
    """
    files: list[tuple[str, str]] = [
        ("agent-stdout.json", turn.stdout or ""),
        ("agent-stderr.log", turn.stderr or ""),
    ]
    with tempfile.TemporaryDirectory() as holder:
        target = Path(holder) / "state"
        copied = exec_command(
            [runtime, "cp", f"{container}:{workdir}/{AGENT_STATE_DIR}", str(target)]
        )
        if copied.ok and target.is_dir():
            for path in sorted(target.rglob("*")):
                if path.is_file():
                    name = f"state/{path.relative_to(target).as_posix()}"
                    files.append((name, path.read_text(encoding="utf-8", errors="replace")))
    if api_key:
        files = [(name, text.replace(api_key, REDACTED_KEY)) for name, text in files]
    return tuple(files)


#: What a logged command line shows instead of the issue text.
ELIDED_PROMPT = "<problem-statement>"

#: What a logged command line shows instead of the provider key.
REDACTED_VALUE = "<redacted>"


def redact_command_line(argv: Sequence[str]) -> str:
    """Render ``argv`` as the one line the episode log keeps.

    Two substitutions, both required by the storage contract:

    * the provider key is replaced by :data:`REDACTED_VALUE`. It is the only
      secret that ever appears on one of these command lines, and the manifest
      records the variable *names* instead of any value.
    * the ``-p`` or ``--topology-task`` argument is replaced by
      :data:`ELIDED_PROMPT`. The issue text
      is already pinned, byte-for-byte, in the pack's suite file; repeating it
      once per episode would bloat the log without adding provenance.
    """
    out: list[str] = []
    elide_next = False
    for token in argv:
        if elide_next:
            out.append(ELIDED_PROMPT)
            elide_next = False
            continue
        if token in ("-p", "--topology-task"):
            out.append(token)
            elide_next = True
            continue
        name, sep, _ = token.partition("=")
        if sep and name == API_KEY_VAR:
            out.append(f"{name}={REDACTED_VALUE}")
            continue
        out.append(token)
    return " ".join(out)
