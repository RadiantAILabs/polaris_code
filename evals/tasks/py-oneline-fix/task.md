# py-oneline-fix

Shape: **one-line fix**. Language: Python. The topology's exact-edit affordance
is language-neutral, so this fixture proves the composition path is not
Rust-specific.

## The problem

`app.py`'s `add` raises `NotImplementedError` instead of returning `a + b`.
`test_app.py` already states the expected behaviour.

## How it is submitted

Through `--topology-task`, as one exact edit with `intent=fast` and a tight
`budget-ms=1000`. Both facts point the same way:
`cosmos::composition::deterministic_pick`'s tight-budget rule
(`crates/polaris_code_core/src/cosmos/composition.rs`)
rules out the extra predict/compare round whenever a stated latency budget is
at or under 5 seconds, and `code.direct-edit` is this build's lowest-latency
registration for a `Fast`-intent single stated edit at cold start
(`tests/topology_selection_learns_from_outcomes.rs`,
`an_empty_record_leaves_the_cold_start_exactly_as_it_was`).

## What a sensible plan looks like

One registration, one recipe, two blocks, no loop: `code.direct-edit` /
`polaris-code.recipe.direct-verified` / `cosmos.verified-step` then
`cosmos.resolve-residuals`. Mutating (it writes the file), not repeated.

## Outcome

`python3 -m unittest discover` passes.
