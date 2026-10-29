"""Building the recorded script a replayed sweep answers from.

A replayed sweep executes nothing, so something has to stand in for every
command the adapter issues. That stand-in is a *script*: an ordered list of
:class:`~evals.core.process.ReplayStep` objects, each matching
a command by substring and answering with the exit status, stdout and — for the
official grader — the report file a real run would have produced.

Writing such a script by hand is possible and unpleasant, so this module builds
one from the pinned suite plus a table of intended outcomes. The adapter's test
suite uses the same builder to assert the exact argv a real sweep would issue.

What a script may *not* do is decide anything. Every number it answers with is
stated by its caller, and :mod:`evals.core.collate` marks any
run whose ``process_runner`` is ``replay`` as a ``plumbing-demonstration``
whatever those numbers say. A rehearsal that could be mistaken for a
measurement would be worse than no rehearsal at all.
"""

from __future__ import annotations

import dataclasses
import json
from typing import Any, Iterable, Mapping, Sequence

from ...core.config import EPISODE_SCHEMA  # noqa: F401 - documents the record this mirrors
from ...core.episode import container_name, session_id
from ...core.process import ReplayStep
from ...core.usage import RUN_USAGE_SCHEMA
from .grader import grader_run_id, model_name
from .suite import Instance, Suite

#: An image digest shape identical to what ``image inspect`` really prints.
PLACEHOLDER_DIGEST = "swebench/sweb.eval.x86_64.example@sha256:" + "0" * 64


@dataclasses.dataclass(frozen=True)
class ScriptedEpisode:
    """The intended outcome of one replayed episode.

    Every field is a decision the *caller* makes. Nothing here is derived from
    a model, a container or a grader, which is exactly why a run assembled from
    these is a rehearsal and not evidence.
    """

    instance_id: str
    mode: str
    repetition: int = 1
    #: The diff ``git diff --cached`` would have returned. Empty means the
    #: agent changed nothing, which SWE-bench scores as unresolved.
    patch: str = ""
    #: ``completed``, ``failed`` or ``interrupted``, as the run reports it.
    status: str = "completed"
    #: Exit code of the agent invocation.
    exit_code: int = 0
    input_tokens: int = 0
    output_tokens: int = 0
    cache_read_tokens: int = 0
    cache_creation_tokens: int = 0
    #: ``None`` reproduces the unpriced-family arm of the usage contract, where
    #: the cost is UNKNOWN rather than zero.
    estimated_cost_usd: float | None = 0.0
    pricing_basis: str = "recorded-price-table"
    #: Whether the official grader would have called this instance resolved.
    resolved: bool = False
    #: When set, the container fails to come up and the agent is never
    #: launched, which is an infrastructure failure rather than a result.
    container_failure: str | None = None
    #: The topology runtime answer carried by the binary's JSON report.
    topology_terminal: str | None = None
    #: The composition policy the binary's JSON report says assembled this
    #: turn's recipe. ``None`` reproduces a turn that started no
    #: runtime-assembled topology; setting it is what makes ``run_result``
    #: emit the ``topology.composition`` object a real composition-axis
    #: episode would report.
    composition_policy: str | None = None
    #: Versioned recipe identity the report would carry alongside the policy.
    composition_recipe: str | None = None
    #: Canonical plan digest the report would carry alongside the policy.
    composition_digest: str | None = None
    #: Whether the report would mark this choice as caller-forced. Defaults
    #: to ``False``: this harness never issues a forcing direction, so an
    #: autonomous choice is the shape every scripted composition episode
    #: reproduces unless a test says otherwise.
    composition_forced: bool = False
    #: Canonical identity of the gate plan admitted by the turn, if any.
    gate_plan_digest: str | None = None
    #: Configuration-independent identity of the admitted gate-plan shape.
    gate_plan_shape_digest: str | None = None
    #: One-based revision attempt of the admitted gate plan.
    gate_plan_revision_attempt: int | None = None
    #: Attempts refused before the admitted gate plan.
    gate_plan_prior_refused_attempts: int | None = None
    #: Attempts refused before fallback to finite composition.
    exhausted_gate_plan_refused_attempts: int | None = None

    @property
    def total_tokens(self) -> int:
        """The sum the usage contract guarantees ``total_tokens`` carries."""
        return (
            self.input_tokens
            + self.output_tokens
            + self.cache_read_tokens
            + self.cache_creation_tokens
        )

    def run_result(self, session: str) -> dict[str, Any]:
        """The ``--output-format json`` object this episode's turn printed."""
        result = {
            "session_id": session,
            "turn_number": 1,
            "run_id": f"run-{self.instance_id}-{self.mode}-{self.repetition}",
            "status": self.status,
            "result": None if self.status != "completed" else "done",
            "error": None if self.status == "completed" else "the turn failed",
            "usage": {
                "schema": RUN_USAGE_SCHEMA,
                "model_id": "anthropic/claude-sonnet-4-6",
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "cache_read_tokens": self.cache_read_tokens,
                "cache_creation_tokens": self.cache_creation_tokens,
                "total_tokens": self.total_tokens,
                "estimated_cost_usd": self.estimated_cost_usd,
                "pricing_basis": self.pricing_basis,
                "pricing": {
                    "family": "sonnet",
                    "input_per_mtok": 3.0,
                    "output_per_mtok": 15.0,
                    "cache_read_per_mtok": 0.3,
                    "cache_write_per_mtok": 3.75,
                },
            },
        }
        if self.topology_terminal is not None:
            result["topology_terminal"] = {"outcome": self.topology_terminal}
        if self.composition_policy is not None:
            result["topology"] = {
                "composition": {
                    "policy": self.composition_policy,
                    "recipe": self.composition_recipe,
                    "digest": self.composition_digest,
                    "forced": self.composition_forced,
                }
            }
        if self.gate_plan_digest is not None:
            result.setdefault("topology", {})["gate_plan"] = {
                "plan_digest": self.gate_plan_digest,
                "shape_digest": self.gate_plan_shape_digest,
                "revision_attempt": self.gate_plan_revision_attempt,
                "prior_refused_attempts": self.gate_plan_prior_refused_attempts,
            }
        if self.exhausted_gate_plan_refused_attempts is not None:
            result.setdefault("topology", {})["exhausted_gate_plan"] = {
                "refused_attempts": self.exhausted_gate_plan_refused_attempts,
            }
        return result


def episode_steps(
    *, instance: Instance, scripted: ScriptedEpisode, eval_run_id: str
) -> list[ReplayStep]:
    """Every command one episode issues, answered.

    The match tokens deliberately include the episode's own container name, so
    two episodes in one script can never answer each other's commands — which
    is how a script would silently merge two cells of the matrix.
    """
    name = container_name(eval_run_id, scripted.mode, scripted.repetition, instance.instance_id)
    session = session_id(eval_run_id, scripted.mode, scripted.repetition, instance.instance_id)
    steps = [
        ReplayStep(match=("image inspect", instance.image), stdout=PLACEHOLDER_DIGEST + "\n"),
    ]
    if scripted.container_failure is not None:
        # `create` is the authoritative check that the image is usable, so a
        # failure there is the shape a real unusable image takes.
        steps.append(
            ReplayStep(
                match=("create --name", name),
                returncode=125,
                stderr=scripted.container_failure,
            )
        )
        steps.append(ReplayStep(match=("rm -f", name)))
        return steps

    steps.extend(
        [
            ReplayStep(match=("create --name", name)),
            ReplayStep(match=("start", name)),
            ReplayStep(match=(name, "mkdir")),
            ReplayStep(match=("cp ", name)),
            ReplayStep(match=(name, "chmod")),
            ReplayStep(match=(name, "rev-parse"), stdout=instance.base_commit + "\n"),
            ReplayStep(match=(name, "status --porcelain"), stdout=""),
            ReplayStep(
                match=(session, "--output-format"),
                returncode=scripted.exit_code,
                stdout=json.dumps(scripted.run_result(session)),
            ),
            ReplayStep(match=(name, "add -A")),
            ReplayStep(match=(name, "diff --cached"), stdout=scripted.patch),
            ReplayStep(match=("rm -f", name)),
        ]
    )
    return steps


def grader_steps(
    *,
    eval_run_id: str,
    mode: str,
    repetition: int,
    scripted: Sequence[ScriptedEpisode],
) -> list[ReplayStep]:
    """The official grader's invocation for one cell, and the report it wrote.

    The report is delivered the way the real harness delivers it — as a file in
    the grader's working directory named after the cell — so the adapter reads
    it back through exactly the code a measured run uses.
    """
    run_id = grader_run_id(eval_run_id, mode, repetition)
    resolved = sorted(e.instance_id for e in scripted if e.resolved)
    empty = sorted(e.instance_id for e in scripted if not e.patch and not e.resolved)
    unresolved = sorted(
        e.instance_id for e in scripted if e.patch and not e.resolved
    )
    report = {
        "resolved_ids": resolved,
        "unresolved_ids": unresolved,
        "empty_patch_instances_ids": empty,
        "error_ids": [],
    }
    return [
        ReplayStep(
            match=("run_evaluation", run_id),
            stdout=f"grading complete for {run_id}\n",
            writes=((f"{model_name(eval_run_id, mode, repetition)}.{run_id}.json",
                     json.dumps(report, indent=2, sort_keys=True)),),
        )
    ]


def build(
    *, suite: Suite, eval_run_id: str, scripted: Iterable[ScriptedEpisode]
) -> list[ReplayStep]:
    """Assemble the whole script for a set of scripted episodes.

    Raises:
        KeyError: if a scripted episode names an instance the pinned suite does
            not hold. A rehearsal of a task that is not in the suite would
            prove nothing about the suite.
    """
    by_id = {instance.instance_id: instance for instance in suite.instances}
    episodes = list(scripted)
    steps: list[ReplayStep] = []
    for episode in episodes:
        steps.extend(
            episode_steps(
                instance=by_id[episode.instance_id], scripted=episode, eval_run_id=eval_run_id
            )
        )
    cells: dict[tuple[str, int], list[ScriptedEpisode]] = {}
    for episode in episodes:
        cells.setdefault((episode.mode, episode.repetition), []).append(episode)
    for (mode, repetition), members in sorted(cells.items()):
        steps.extend(
            grader_steps(
                eval_run_id=eval_run_id, mode=mode, repetition=repetition, scripted=members
            )
        )
    return steps


def to_json(steps: Sequence[ReplayStep]) -> str:
    """Serialize a script to the form ``--replay`` reads."""
    return json.dumps(
        [_step_json(step) for step in steps], indent=2, sort_keys=True, ensure_ascii=False
    ) + "\n"


def _step_json(step: ReplayStep) -> Mapping[str, Any]:
    """One step's JSON form, omitting everything left at its default."""
    out: dict[str, Any] = {"match": list(step.match)}
    if step.returncode:
        out["returncode"] = step.returncode
    if step.stdout:
        out["stdout"] = step.stdout
    if step.stderr:
        out["stderr"] = step.stderr
    if step.timed_out:
        out["timed_out"] = step.timed_out
    if step.writes:
        out["writes"] = [[name, body] for name, body in step.writes]
    return out
