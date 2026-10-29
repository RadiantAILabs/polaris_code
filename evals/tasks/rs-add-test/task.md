# rs-add-test

Shape: **add a test**.

## The problem

`is_even` has no tests at all.

## How it is submitted

Conversationally, through `--print`. The gate-block catalogue
(`crates/polaris_code/src/cosmos/gate_blocks/`) has a `code.create-file`
block for exactly this task shape, which a plan can reach through the
planner-driven `gates` composition policy. This fixture states the task in
prose and does not direct it at a topology, so it runs the ordinary agent loop.

## What a sensible plan looks like

None — see above. `report.topology` is expected to be `null`.

## Outcome

`cargo test` passes and the crate has at least two tests (checked by counting
`cargo test -- --list`, not by grepping for a specific test name, so the
checker does not dictate the agent's naming).
