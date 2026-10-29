"""A small, model-free measuring stick for one claim: the polaris-code agent
composes a different graph for a different task.

This package is deliberately separate from :mod:`evals.core`/:mod:`evals.packs`.
Those own SWE-bench-shaped accuracy measurement (a dataset, an official grader,
a resolved-rate). This package owns something narrower and cheaper: for each of
a handful of small fixture workspaces under ``evals/tasks/``, drive the shipped
``polaris-code`` binary through whichever headless entry point the task shape
actually reaches, read back the plan it ran under from the binary's own
``--output-format json`` report, and check two independent things — did the
work land, and does the plan the binary chose look like what a sensible plan
for that task shape looks like. Neither question needs a model to check, even
though answering some of the *tasks* does.

See ``evals/README.md`` for how to run it and what each fixture probes.
"""

from __future__ import annotations
