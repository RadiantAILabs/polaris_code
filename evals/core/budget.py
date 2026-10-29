"""The cost-ceiling guard, shared by every dataset pack.

The guard is the core's; the *number* is a per-dataset default, supplied
by the pack (:meth:`~evals.core.pack.DatasetPack.default_api_cost_ceiling_usd`)
and overridable per run. The ceiling is enforced in two places, not one:

* :meth:`CostGuard.check_projection` refuses to **start** a run whose projected
  spend exceeds the ceiling. This stops an unexpectedly large run from being
  started by accident — a mistyped ``--repetitions 3`` is refused
  before the first container starts, not discovered at episode 300.
* :meth:`CostGuard.may_schedule` stops **scheduling** new episodes once the
  measured spend has reached the ceiling. Episodes already started are allowed
  to finish and are retained; the run is then marked incomplete and emits no
  primary score.

The one sharp edge in the usage contract is handled here. ``estimated_cost_usd``
is ``null`` for an unpriced model family, and ``usage`` itself is ``null`` for
an unmeasured run. Both mean UNKNOWN, never zero: an unpriced model that was
read as free would run the whole matrix at an unbounded true cost. A run that
reports an unknown cost therefore halts scheduling exactly as a run at the
ceiling does, and says so.

UNKNOWN is not the same as *impossible*, which is why :meth:`CostGuard.record`
takes ``billable``. An episode whose container could not even be created never
launched the agent, so no provider call was reachable and its cost is known to
be zero rather than unknown. Collapsing the two would let a single image-pull
failure halt a sweep that should retry the attempt and carry on with the cell
marked incomplete.
"""

from __future__ import annotations

import dataclasses

from .config import PLANNING_COST_PER_EPISODE_USD
from .usage import RunUsage


class BudgetExceeded(RuntimeError):
    """A run was refused before starting because its projection is too large."""


#: Why scheduling stopped, recorded in the summary and the manifest.
HALT_CEILING = "api-cost-ceiling-reached"
HALT_UNKNOWN_COST = "unknown-episode-cost"


@dataclasses.dataclass
class CostGuard:
    """Tracks measured spend against a hard ceiling.

    Mutable by design: it is the one object that accumulates across a sweep.
    """

    #: The hard ceiling in USD.
    ceiling_usd: float
    #: Planning cost of one episode, used only for the pre-run projection.
    planning_cost_per_episode_usd: float = PLANNING_COST_PER_EPISODE_USD
    #: Measured spend so far, summed from episodes that reported a cost.
    spent_usd: float = 0.0
    #: Episodes whose cost could not be known.
    unknown_cost_episodes: int = 0
    #: Why scheduling halted, or ``None`` while the run may continue.
    halt_reason: str | None = None

    def check_projection(self, episodes: int) -> float:
        """Project ``episodes`` at the planning rate and refuse an overrun.

        Returns:
            The projected spend, so a caller can record it in the manifest.

        Raises:
            BudgetExceeded: if the projection exceeds the ceiling. The message
                names both numbers and the flag that raises the ceiling.
        """
        if episodes < 0:
            raise ValueError(f"episode count must not be negative, got {episodes}")
        projected = episodes * self.planning_cost_per_episode_usd
        if projected > self.ceiling_usd:
            raise BudgetExceeded(
                f"projected API cost ${projected:.2f} for {episodes} episodes exceeds the "
                f"${self.ceiling_usd:.2f} ceiling; reduce --instances, --modes or "
                f"--repetitions, or set an appropriate --api-cost-ceiling-usd"
            )
        return projected

    def check_projection_by_mode(
        self, per_mode_episodes: dict[str, int], rate_by_mode: dict[str, float]
    ) -> float:
        """Like :meth:`check_projection`, but each mode may cost a different rate.

        The composition axis's ``model`` cell can spend more per episode than
        its ``deterministic`` cell (the model chooser's own bounded worst
        case, on top of the ordinary planning cost); summing one flat rate
        over every episode would under-count what a sweep with a ``model``
        cell can actually spend. ``rate_by_mode`` supplies each mode's own
        rate; a mode missing from it falls back to
        ``self.planning_cost_per_episode_usd`` — the same default
        :meth:`check_projection` uses uniformly.

        Args:
            per_mode_episodes: episode count per mode. ``instances ×
                repetitions`` for each mode in a composition sweep, since
                every mode runs the same instances the same number of times.
            rate_by_mode: per-episode planning rate for each mode.

        Returns:
            The projected spend, so a caller can record it in the manifest.

        Raises:
            ValueError: if any episode count is negative.
            BudgetExceeded: if the summed projection exceeds the ceiling.
        """
        if any(count < 0 for count in per_mode_episodes.values()):
            raise ValueError(f"episode count must not be negative, got {per_mode_episodes}")
        total_episodes = sum(per_mode_episodes.values())
        projected = sum(
            count * rate_by_mode.get(mode, self.planning_cost_per_episode_usd)
            for mode, count in per_mode_episodes.items()
        )
        if projected > self.ceiling_usd:
            raise BudgetExceeded(
                f"projected API cost ${projected:.2f} for {total_episodes} episodes exceeds the "
                f"${self.ceiling_usd:.2f} ceiling; reduce --instances, --modes or "
                f"--repetitions, or set an appropriate --api-cost-ceiling-usd"
            )
        return projected

    def record(self, usage: RunUsage | None, *, billable: bool = True) -> None:
        """Fold one finished episode's accounting into the running total.

        ``usage is None`` (unmeasured) and ``estimated_cost_usd is None``
        (unpriced family) are both counted as unknown, and either one halts
        further scheduling. Nothing is inferred and nothing is estimated: the
        adapter would rather stop than keep spending against a number it cannot
        read.

        Args:
            usage: the episode's own accounting, or ``None`` when it reported
                none.
            billable: whether the agent was actually launched. ``False`` means
                no provider call was reachable — the container never came up —
                so the episode cost a known zero and neither adds to the total
                nor counts as unknown. Only the caller knows this; the absence
                of a usage record cannot distinguish the two.
        """
        if not billable:
            return
        if usage is None or usage.estimated_cost_usd is None:
            self.unknown_cost_episodes += 1
            if self.halt_reason is None:
                self.halt_reason = HALT_UNKNOWN_COST
            return
        self.spent_usd += usage.estimated_cost_usd
        if self.spent_usd >= self.ceiling_usd and self.halt_reason is None:
            self.halt_reason = HALT_CEILING

    def may_schedule(self) -> bool:
        """Whether another episode may be started."""
        return self.halt_reason is None

    @property
    def cost_is_complete(self) -> bool:
        """Whether every finished episode contributed a knowable cost."""
        return self.unknown_cost_episodes == 0

    def to_json(self) -> dict[str, object]:
        """The summary projection of the guard's final state."""
        return {
            "ceiling_usd": self.ceiling_usd,
            "planning_cost_per_episode_usd": self.planning_cost_per_episode_usd,
            "measured_cost_usd": round(self.spent_usd, 6),
            "unknown_cost_episodes": self.unknown_cost_episodes,
            "cost_is_complete": self.cost_is_complete,
            "halt_reason": self.halt_reason,
        }
