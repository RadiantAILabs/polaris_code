"""Terminal-Bench 2.0's Harbor adapter.

This is deliberately not a :class:`evals.core.pack.DatasetPack`: Harbor owns
the trial lifecycle, task environments, verifier and result records.  The
adapter is an installed Harbor agent that changes the task filesystem, rather
than a Polaris evaluation episode that returns a patch to this repository's
shared harness.
"""

