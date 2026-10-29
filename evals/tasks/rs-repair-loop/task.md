# rs-repair-loop

Shape: **bug with a failing test to reproduce then fix** (repair loop).

## The problem

`clamp` forgets the upper bound (`x.max(lo)` instead of `x.max(lo).min(hi)`),
so `clamps_above_high_bound` fails while the other two tests happen to pass.

## How it is submitted

Conversationally, through `--print`, not through `--topology-task`. The prompt
states the problem in prose, and the ordinary conversational agent loop
reproduces the failure and fixes it using its own tools (`run_command`,
`edit_file`). A topology spec can instead gate its verdict on a real test run:
`command=` (with `command-args=`) builds `SuccessCheck::Command`
(`crates/polaris_code_core/src/cosmos/task.rs`), and a stated command is what
makes a `ProjectTestRunner` available to `code.test-gated-edit`
(`effective_host_capabilities` in `cosmos/host.rs`). This fixture does not use
that route; it measures the conversational loop on a failing-test repair.

## What a sensible plan looks like

None — see above. `report.topology` is expected to be `null`. This fixture is
the conversational baseline for the repair shape.

## Outcome

`cargo test` passes, without the tests themselves having changed.
