"""Out-of-process evaluation for ``polaris-code``.

Everything under ``evals/`` is *outside* the shipped agent. Code here may
launch the released ``polaris-code`` binary as a subprocess, orchestrate
containers and call an external grader; it may not import
``polaris_code_core``, ``SessionsAPI``, ``GraphExecutor``, ``CodeAgent`` or any
model API, and the harness adds no benchmark text to the agent's prompt.

Two halves, and the line between them is the point:

``core``   what belongs to *evaluation*: run identity, accounting, the budget
           guard, the statistics, the evidence bar, the conclusion labels,
           immutability, the replay seam and the command line. It names no
           dataset.
``packs``  one subpackage per dataset: its suite list, its denominator, its
           environment provider, its grader adapter, and its split and licence
           policy. See :mod:`evals.core.pack` for the contract.

Adding a benchmark is adding a pack. A change under ``core`` that a new dataset
forces is a sign the contract is wrong, not that the core needs a special case.
"""
