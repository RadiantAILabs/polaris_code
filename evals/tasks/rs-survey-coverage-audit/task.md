# rs-survey-coverage-audit

Shape: **survey-then-answer over many files**.

## The problem

Three files (`src/alpha.rs`, `src/beta.rs`, `src/gamma.rs`) each have one
unresolved `todo!()` stub. Nothing states which one to fix; the ask is a
read-only survey of the whole crate, then a written answer.

## How it is submitted

Conversationally, through `--print`. `code.coverage-audit` is this build's
own read-only many-site audit registration
(`coverage_audit_graph` in `crates/polaris_code_core/src/cosmos/graphs.rs`,
built on `polaris_cosmos::cognition::coverage_sweep::build`). This fixture
states the request in prose and does not direct it at that registration, so it
runs the ordinary conversational agent loop. It can be re-pointed at
`topology=code.coverage-audit` with a statement-form spec.

## What a sensible plan looks like

None — see above. `report.topology` is expected to be `null`.

## Outcome

The turn completes and its answer mentions every one of the three files with
an unresolved stub (checked as three independent substrings, not a fixed
wording, since a conversational answer's phrasing is not otherwise
constrained).
