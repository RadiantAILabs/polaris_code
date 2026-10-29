"""The dataset packs, and the registry that names them.

One subpackage per benchmark dataset. Each holds everything that is true of
that dataset and of nothing else: its pinned suite, the environment its
episodes run in, its grader adapter, its split and licence policy, and the
default cost ceiling for a sweep of it.

Nothing here is imported by :mod:`evals.core`. The dependency runs one way —
a pack imports the core, the core is handed a pack — which is what makes
"adding a dataset" an addition rather than an edit.

Registering a pack
------------------

Add its subpackage, then add one line to :data:`PACKS`. The name in that
mapping is the name recorded in every manifest as ``config.dataset_pack``, so
it is a stable identifier: renaming it would make an existing run directory
unrecomputable under its own pack.

:func:`load` exists for a caller that has a name and wants the pack. A pack's
own ``__main__`` does not need it — it constructs its pack directly, so a
broken unrelated pack cannot stop it from running.
"""

from __future__ import annotations

from typing import Callable

from ..core.pack import DatasetPack


def _swebench_multilingual() -> DatasetPack:
    """Construct the SWE-bench Multilingual pack, imported on demand."""
    from .swebench_multilingual.pack import SwebenchMultilingualPack

    return SwebenchMultilingualPack()


#: Every registered pack, by the name its manifests record. Constructors rather
#: than instances, so importing this module costs nothing and one pack whose
#: pinned file is missing does not break another pack's command line.
PACKS: dict[str, Callable[[], DatasetPack]] = {
    "swebench-multilingual": _swebench_multilingual,
}


class UnknownPack(KeyError):
    """A pack name that is not registered in :data:`PACKS`."""


def load(name: str) -> DatasetPack:
    """The registered pack called ``name``.

    Raises:
        UnknownPack: if no pack is registered under that name. The message
            lists the registered names, because the usual cause is a run
            directory whose ``config.dataset_pack`` names a pack that is not
            registered.
    """
    try:
        construct = PACKS[name]
    except KeyError as err:
        raise UnknownPack(
            f"no dataset pack named {name!r}; registered: {', '.join(sorted(PACKS))}"
        ) from err
    return construct()
