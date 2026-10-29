"""The dataset-neutral evaluation harness.

Everything in this package is true of *any* benchmark we might run the shipped
``polaris-code`` binary against. Nothing here names a dataset, a language, a
container image or a grader. What one dataset decides lives in a pack under
:mod:`evals.packs`; :mod:`evals.core.pack` states exactly what a pack must
supply.

What the core owns
------------------

``config``      run identity, the mode matrix, the evidence bar, timeouts and
                the whole recorded :class:`~evals.core.config.RunConfig`.
``pack``        the contract a dataset pack implements, and nothing else.
``process``     the replay seam: one ``ProcessRunner`` for every command.
``episode``     the episode record, the termination vocabulary and the one
                agent invocation.
``usage``       the ``polaris-code-run-usage/v1`` accounting passthrough.
``budget``      the cost-ceiling guard, for which ``null`` means UNKNOWN.
``report``      the per-cell disposition vocabulary the evidence bar reads.
``statistics``  the deterministic paired task-cluster bootstrap.
``collate``     ``summary.json``, derived and byte-reproducible.
``manifest``    ``polaris-code-harness-eval/v1`` and the output digests.
``runner``      the sweep orchestrator and the ``recompute`` seam.
``cli``         ``run``, ``recompute`` and ``preflight``, driven by a pack.

Version
-------

``__version__`` is recorded in every manifest. The manifest's ``pack`` member
and ``config.dataset_pack`` field are additive: a run directory that lacks
them still recomputes byte-for-byte.
"""

__all__ = ["__version__"]

#: Version of the evaluation core, recorded in every manifest.
__version__ = "1.1.0"
