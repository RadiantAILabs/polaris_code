# rs-careful-verify-edit

Shape: the same one-line change, composed differently. It demonstrates that
the *same* edit can compose into a materially different graph purely from the
requester's stated intent and success check, with no change to the edit
itself.

## The problem

Identical shape to `py-oneline-fix`, in Rust: `add` is a `todo!()` stub, and a
unit test in the same file states the expected behaviour.

## How it is submitted

The same exact-edit form as `py-oneline-fix`, but `intent=careful` and no
`budget-ms`. `cosmos::composition::deterministic_pick`
(`crates/polaris_code_core/src/cosmos/composition.rs`) escalates to the
predict/apply/verify/compare loop whenever the success check is
`stated-edits` (this graph's own verify is the only check the work will ever
get) and no tight latency budget rules it out — exactly this fixture's shape.
Selection also independently favours `code.prediction-error-edit` for a
`Careful` intent at cold start
(`tests/topology_selection_learns_from_outcomes.rs`,
`an_empty_record_leaves_the_cold_start_exactly_as_it_was`), and
`work_submitted_through_the_command_reaches_the_queue_and_runs` in
`tests/topology_work_affordance.rs` submits this identical spec shape
(`intent=careful`, no budget) and asserts the same registration.

## What a sensible plan looks like

`code.prediction-error-edit` / `polaris-code.recipe.prediction-error` /
`cosmos.prediction-error-loop` — one block because the whole predict/apply/
verify/compare shape is one opaque, already-loop-shaped Cosmos block, not the
two-block sequence the leaner recipe uses.

## Outcome

`cargo test` passes.
