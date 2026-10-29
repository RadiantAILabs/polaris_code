# rs-ambiguous-kind

Shape: **a task with an ambiguous kind**.

## The problem

`parse_all` silently drops anything it cannot parse. Whether that is a bug, a
documented feature, or a matter of taste is exactly what is left unstated.

## How it is submitted

Conversationally, through `--print`, with a deliberately open statement that
names no file, no exact edit and no question shape
`CodingQuestion::parse` recognises (`cosmos/payload.rs`). It does not
match `cosmos::host::auto_route`'s lexical prefixes either, so it is not even
quietly promoted to the light read-only path — it reaches the ordinary
conversational agent loop with no topology decision made at all, which is the
honest behaviour for a request that does not yet say what kind of work it is.

## What a sensible plan looks like

None — `report.topology` is expected to be `null`. This fixture's own outcome
bar is deliberately lenient: it does not prescribe *what* the agent should
conclude (report back, propose a change, or make one), only that whatever it
does leaves the crate in a working state.

## Outcome

`cargo test` still passes.
