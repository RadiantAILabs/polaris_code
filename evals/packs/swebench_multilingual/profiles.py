"""This pack's pinned fast configurations.

The machinery, the label and the reasons a fast run can never be evidence are
:mod:`evals.core.profiles`. What lives here is the only dataset-specific part:
which of *this* suite's instances a named profile attempts.
"""

from __future__ import annotations

from typing import Mapping

from ...core.profiles import IterationProfile

#: The pinned fast subset: the first instance of each of the suite's seven
#: repositories, in the suite's own ascending instance-id order.
#:
#: Chosen for *coverage per dollar*, not for difficulty. Each repository is a
#: distinct official image with its own workspace layout and build, and that is
#: where harness breakage actually appears; a second instance from a repository
#: already covered adds almost nothing to that signal and costs another episode.
#: The within-repository pick is the lowest id rather than the easiest task,
#: deliberately: an iteration profile that selected tasks the agent tends to
#: solve would drift towards a flattering number, and a flattering number is the
#: thing the whole mechanism exists to prevent.
RUST_SMOKE_7 = IterationProfile(
    name="rust-smoke-7",
    instance_ids=(
        "astral-sh__ruff-15309",
        "burntsushi__ripgrep-2209",
        "nushell__nushell-12901",
        "sharkdp__bat-1892",
        "tokio-rs__axum-1119",
        "tokio-rs__tokio-4384",
        "uutils__coreutils-6377",
    ),
    repetitions=1,
    episode_timeout_seconds=600,
)

#: Every profile this pack's ``run`` command accepts, keyed by name.
PROFILES: Mapping[str, IterationProfile] = {RUST_SMOKE_7.name: RUST_SMOKE_7}

#: The profile to reach for when nobody has a reason to prefer another.
DEFAULT_PROFILE = RUST_SMOKE_7.name

#: Profile names in a stable order, for the command's own help text.
PROFILE_NAMES: tuple[str, ...] = tuple(sorted(PROFILES))
