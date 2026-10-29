# rs-read-only-question

Shape: **read-only question about the code**.

## The problem

Nothing to fix — the point is that answering does not touch the tree.

## How it is submitted

Through `--topology-task`, as `statement="what is in src/lib.rs" intent=answer`.
`CodingQuestion::parse` recognises the `what is in <path>` prefix
(`crates/polaris_code_core/src/cosmos/payload.rs`) as an
[`Inspect`](../../../crates/polaris_code_core/src/cosmos/payload.rs) question.
`intent=answer` forbids every source-mutating registration on the hard-
constraint channel (the `SOURCE_MUTATION` constraint in `cosmos/task.rs`), and
the deterministic selector returns `code.tree-question` first under that
objective at cold start (`tests/answer_shape_proof.rs`,
`the_answer_shape_wins_an_objective_that_forbids_source_mutation`).

## What a sensible plan looks like

`code.tree-question` / `polaris-code.recipe.tree-question` /
`polaris-code.tree-survey` — no mutating block, no loop. The workspace is
byte-identical before and after.

## Outcome

`status` is `completed`, the answer says the topology resolved the question
(one `Inspect` finding, `Status::Pass`), and no file changed.
