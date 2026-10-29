"""Reading the run's own accounting, and never recomputing it.

``polaris-code --output-format json`` prints one object whose ``usage`` member
is a ``polaris-code-run-usage/v1`` record produced by the agent's own
``TokenMeter``. This module parses exactly that and nothing else. Recomputing
token pricing in the adapter would create a second accounting authority, so
there is deliberately no price table here — the rates that priced a run arrive
inside the record and are stored with it.

Three rules the rest of the adapter depends on:

1. An unrecognised ``schema`` is refused, not coerced. That is what the version
   field is for.
2. ``usage: null`` means UNMEASURED. It is not zero and not free.
3. ``estimated_cost_usd: null`` means the model family is not priced. It is
   likewise not zero; :mod:`evals.core.budget` treats it as an
   unknown that stops a run rather than as a free episode.
"""

from __future__ import annotations

import dataclasses
import json
from typing import Any, Mapping

#: The only accounting schema this adapter accepts.
RUN_USAGE_SCHEMA = "polaris-code-run-usage/v1"


class UsageError(ValueError):
    """The run's stdout was not a readable result object."""


def _parse_num_turns(value: Any) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise UsageError("num_turns must be a nonnegative integer")
    return value


@dataclasses.dataclass(frozen=True)
class RunUsage:
    """One run's billed tokens and estimated cost, as the run reported them."""

    model_id: str
    input_tokens: int
    output_tokens: int
    cache_read_tokens: int
    cache_creation_tokens: int
    total_tokens: int
    #: ``None`` when the model family is not priced. NEVER read as zero.
    estimated_cost_usd: float | None
    pricing_basis: str
    #: The matched rate card, or ``None``. Stored verbatim so an episode record
    #: prices itself without this adapter or the crate.
    pricing: Mapping[str, Any] | None

    #: Model request attempts; absent in older binaries, never inferred from tokens.
    num_turns: int | None = None

    @property
    def cost_is_known(self) -> bool:
        """Whether this run contributed a knowable amount to the ceiling."""
        return self.estimated_cost_usd is not None

    def to_json(self) -> dict[str, Any]:
        """The episode-record projection, field-for-field with the source."""
        return {
            "schema": RUN_USAGE_SCHEMA,
            **({"num_turns": self.num_turns} if self.num_turns is not None else {}),
            "model_id": self.model_id,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_creation_tokens": self.cache_creation_tokens,
            "total_tokens": self.total_tokens,
            "estimated_cost_usd": self.estimated_cost_usd,
            "pricing_basis": self.pricing_basis,
            "pricing": dict(self.pricing) if self.pricing is not None else None,
        }

    @classmethod
    def parse(cls, raw: Mapping[str, Any]) -> "RunUsage":
        """Build a :class:`RunUsage` from the ``usage`` member.

        Raises:
            UsageError: if the schema is not :data:`RUN_USAGE_SCHEMA`, if a
                counter is missing, or if ``total_tokens`` does not equal the
                sum of the four tiers. The crate guarantees that identity; a
                violation means the record was edited or came from a different
                producer, and either way it must not be collated.
        """
        schema = raw.get("schema")
        if schema != RUN_USAGE_SCHEMA:
            raise UsageError(
                f"unsupported usage schema {schema!r}: this adapter reads {RUN_USAGE_SCHEMA!r}"
            )
        try:
            tiers = (
                int(raw["input_tokens"]),
                int(raw["output_tokens"]),
                int(raw["cache_read_tokens"]),
                int(raw["cache_creation_tokens"]),
            )
            total = int(raw["total_tokens"])
            basis = str(raw["pricing_basis"])
            model_id = str(raw["model_id"])
        except KeyError as err:
            raise UsageError(f"usage record is missing {err}") from err

        if total != sum(tiers):
            raise UsageError(
                f"usage record is inconsistent: total_tokens {total} != sum of tiers {sum(tiers)}"
            )

        cost = raw.get("estimated_cost_usd")
        pricing = raw.get("pricing")
        return cls(
            num_turns=_parse_num_turns(raw.get("num_turns")),
            model_id=model_id,
            input_tokens=tiers[0],
            output_tokens=tiers[1],
            cache_read_tokens=tiers[2],
            cache_creation_tokens=tiers[3],
            total_tokens=total,
            estimated_cost_usd=None if cost is None else float(cost),
            pricing_basis=basis,
            pricing=None if pricing is None else dict(pricing),
        )


@dataclasses.dataclass(frozen=True)
class RunResult:
    """The whole ``--output-format json`` object one episode's turn printed."""

    session_id: str
    turn_number: int
    run_id: str | None
    #: ``completed``, ``failed`` or ``interrupted``.
    status: str
    result: str | None
    error: str | None
    #: ``None`` when the run installed no meter or never reached a session.
    usage: RunUsage | None
    #: The ``max_tokens`` every request of the turn carried. Configuration of
    #: the run, not accounting, so it sits beside ``usage`` rather than inside
    #: the versioned record — which is also why it is read leniently: ``None``
    #: means the agent that produced this stdout predates the field, not that
    #: the run was unbounded.
    max_output_tokens: int | None
    #: Topology-path terminal answer, absent for a conversational turn.
    topology_terminal: str | None
    #: The composition policy the run's own report says assembled this turn's
    #: recipe, read from ``topology.composition.policy``. ``None`` when the
    #: turn started no runtime-assembled topology.
    composition_policy: str | None
    #: Versioned recipe identity from ``topology.composition.recipe``.
    composition_recipe: str | None
    #: Canonical plan digest from ``topology.composition.digest``.
    composition_digest: str | None
    #: Whether an explicit caller direction forced the choice, from
    #: ``topology.composition.forced``.
    composition_forced: bool | None
    #: Canonical identity of an admitted gate-expression plan, from
    #: ``topology.gate_plan.plan_digest``.
    gate_plan_digest: str | None
    #: Configuration-independent identity of the gate-plan shape.
    gate_plan_shape_digest: str | None
    #: Short human-readable rendering of the admitted gate-plan shape.
    gate_plan_shape: str | None
    #: Gate planner identity that assembled the admitted shape.
    gate_plan_planner: str | None
    #: One-based gate-plan revision attempt that admitted.
    gate_plan_revision_attempt: int | None
    #: Earlier refused attempts before the admitted gate plan.
    gate_plan_prior_refused_attempts: int | None
    #: Refused attempts before finite-recipe fallback.
    exhausted_gate_plan_refused_attempts: int | None

    @classmethod
    def parse(cls, stdout: str) -> "RunResult":
        """Parse one agent invocation's stdout.

        Raises:
            UsageError: if stdout is not a single JSON object, or if its
                ``usage`` member is present but unreadable.
        """
        text = stdout.strip()
        if not text:
            raise UsageError("agent produced no stdout to parse as JSON")
        try:
            raw = json.loads(text)
        except json.JSONDecodeError as err:
            raise UsageError(f"agent stdout is not JSON: {err}") from err
        if not isinstance(raw, dict):
            raise UsageError(f"agent stdout is {type(raw).__name__}, expected one object")

        usage = raw.get("usage")
        limit = raw.get("max_output_tokens")
        topology_terminal_raw = raw.get("topology_terminal")
        terminal = None
        if isinstance(topology_terminal_raw, dict) and topology_terminal_raw.get("outcome") is not None:
            terminal = str(topology_terminal_raw["outcome"])

        topology = raw.get("topology")
        composition = topology.get("composition") if isinstance(topology, dict) else None
        comp_policy = comp_recipe = comp_digest = None
        comp_forced = None
        if isinstance(composition, dict):
            comp_policy = None if composition.get("policy") is None else str(composition["policy"])
            comp_recipe = None if composition.get("recipe") is None else str(composition["recipe"])
            comp_digest = None if composition.get("digest") is None else str(composition["digest"])
            comp_forced = None if composition.get("forced") is None else bool(composition["forced"])

        gate_plan = topology.get("gate_plan") if isinstance(topology, dict) else None
        exhausted_gate_plan = topology.get("exhausted_gate_plan") if isinstance(topology, dict) else None
        gate_plan_digest = gate_plan_shape_digest = gate_plan_shape = gate_plan_planner = None
        gate_plan_revision_attempt = gate_plan_prior_refused_attempts = None
        exhausted_gate_plan_refused_attempts = None
        if isinstance(gate_plan, dict):
            gate_plan_digest = None if gate_plan.get("plan_digest") is None else str(gate_plan["plan_digest"])
            gate_plan_shape_digest = None if gate_plan.get("shape_digest") is None else str(gate_plan["shape_digest"])
            gate_plan_shape = None if gate_plan.get("shape") is None else str(gate_plan["shape"])
            gate_plan_planner = None if gate_plan.get("planner") is None else str(gate_plan["planner"])
            gate_plan_revision_attempt = (
                None if gate_plan.get("revision_attempt") is None else int(gate_plan["revision_attempt"])
            )
            gate_plan_prior_refused_attempts = (
                None
                if gate_plan.get("prior_refused_attempts") is None
                else int(gate_plan["prior_refused_attempts"])
            )
        if isinstance(exhausted_gate_plan, dict):
            exhausted_gate_plan_refused_attempts = (
                None
                if exhausted_gate_plan.get("refused_attempts") is None
                else int(exhausted_gate_plan["refused_attempts"])
            )

        return cls(
            max_output_tokens=None if limit is None else int(limit),
            session_id=str(raw.get("session_id", "")),
            turn_number=int(raw.get("turn_number", 0)),
            run_id=None if raw.get("run_id") is None else str(raw["run_id"]),
            status=str(raw.get("status", "")),
            result=None if raw.get("result") is None else str(raw["result"]),
            error=None if raw.get("error") is None else str(raw["error"]),
            usage=None if usage is None else RunUsage.parse(usage),
            topology_terminal=terminal,
            composition_policy=comp_policy,
            composition_recipe=comp_recipe,
            composition_digest=comp_digest,
            composition_forced=comp_forced,
            gate_plan_digest=gate_plan_digest,
            gate_plan_shape_digest=gate_plan_shape_digest,
            gate_plan_shape=gate_plan_shape,
            gate_plan_planner=gate_plan_planner,
            gate_plan_revision_attempt=gate_plan_revision_attempt,
            gate_plan_prior_refused_attempts=gate_plan_prior_refused_attempts,
            exhausted_gate_plan_refused_attempts=exhausted_gate_plan_refused_attempts,
        )
