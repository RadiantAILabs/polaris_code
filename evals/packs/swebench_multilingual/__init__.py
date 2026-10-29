"""The SWE-bench Multilingual Rust dataset pack.

Runs the pinned 43-instance Rust slice of SWE-bench Multilingual through the
shipped non-interactive ``polaris -p``
entry point once per ``POLARIS_CODE_GRAPH_SEARCH`` mode, grade the collected
patches with the official Docker grader, and collate an immutable evidence
record whose primary number is ``100 * resolved / 43`` per mode.

The pack owns this dataset only. It does not score, does not prompt, does not
drive a second agent loop and does not price tokens: the official grader owns
correctness, the agent's own ``TokenMeter`` owns accounting, and
:mod:`evals.core` owns everything that would be identical for another dataset.

Module map
----------

``pack``         :class:`~evals.packs.swebench_multilingual.pack.SwebenchMultilingualPack`
                 — the one object the core harness is handed.
``suite``        the pinned instance list, its denominator and its revision lock.
``profiles``     this slice's named, pinned fast subsets for iterating between
                 sweeps, structurally barred from reaching a scored conclusion.
``environment``  one episode in a disposable container at ``/testbed``.
``grader``       the official SWE-bench grader invocation and its report.
``replay_script``  builds scripts for deterministic, zero-spend replay tests.

Everything else — run identity, accounting, the budget guard, the statistics,
the evidence bar, the conclusion labels, immutability, the replay seam and the
command line — is in :mod:`evals.core`.
"""

__all__ = ["__version__"]

#: Version of this pack's pinned content and adapters, recorded in every
#: manifest under ``pack.version``. The core records its own version
#: separately, under ``adapter_version``. The pinned iteration profiles are
#: part of this version; the label a profile earns is the core's.
__version__ = "1.1.0"
