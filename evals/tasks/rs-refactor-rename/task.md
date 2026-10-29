# rs-refactor-rename

Shape: **refactor/rename across several files**.

## The problem

`rectangle_area` is defined in `src/shapes.rs`, re-exported from `src/lib.rs`,
and called from `src/consumer.rs`. Rename it to `area_of_rectangle`
everywhere, keeping the crate compiling and its one test passing.

## How it is submitted

Conversationally, through `--print`. A `--topology-task` spec states every
exact edit up front (`file=`/`find=`/`replace=`, plus numbered
`file2=`/`find2=`/`replace2=` triples; see `FIELDS` and `MAX_EDITS` in
`crates/polaris_code_core/src/terminal/topology.rs`), so it suits a rename only
when the caller already knows every site. This fixture leaves the agent to
find the sites, so it runs the ordinary agent loop, which finds every call site
with its own search tools.

## What a sensible plan looks like

None — see above. `report.topology` is expected to be `null`.

## Outcome

Every occurrence of `rectangle_area` is gone from `src/`, `area_of_rectangle`
appears in all three files, and `cargo test` passes.
