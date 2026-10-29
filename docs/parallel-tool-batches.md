# Parallel read-only tool batches

When the model asks for four files in one assistant message, `polaris-code` used
to fetch them one at a time. Each read is io-bound and independent of the
others, so the turn cost four round trips of latency where it needed one. This
note records what changed, what deliberately did not, and how the guarantees
that could have been broken are held.

`tests/parallel_readonly_batch.rs` drives the real agent loop with a scripted
provider and delayed tools. Its concurrency assertion is expressed relative to
the slowest call rather than as a machine-dependent speedup ratio.

## The lane test

`agents::code_agent::batch_runs_concurrently` decides, and it judges the batch
as a whole:

1. **More than one call.** A single call is the sequential loop's own case.
2. **Every call is read-only**, per `tools::tool_is_read_only`.
3. **Every call resolved to `ToolPermission::Allow`.**

Anything else — one mutating call, one confirmation, one denial — takes the
sequential loop, unchanged.

Splitting a mixed batch into a concurrent read-only prefix and a sequential tail
was considered and rejected. It requires reasoning about which reads commute
with the write that follows them; `[read a, edit a, read a]` is the
counter-example that kills it, and the residual win does not pay for a rule that
subtle.

Requiring `Allow` across the whole batch is what keeps confirmation from
becoming racy. A `Confirm` anywhere means a human is in the loop, and
interleaving an approval dialog with calls already racing behind it is exactly
the confusion worth refusing.

## Where read-only-ness is declared

`tools::tool_is_read_only` — sited with the toolsets, not in the agent loop. The
loop asks; it does not keep its own table.

The predicate is **fail-closed**: a name it has never heard of answers `false`,
so the batch containing it takes the sequential lane. Forgetting to classify a
new tool costs the latency this change saves and nothing else. A new tool is
never wrongly raced.

`tests/read_only_declaration.rs` makes the forgetting loud. It boots the tool
plugins `main.rs` boots, asks the `ToolRegistry` what is registered, and
compares that set against a hand-written table. Add a tool without classifying
it and the test names it.

A predicate over names is not where this property ultimately belongs. It can
move to tool metadata or TBox capability triples once the framework exposes a
read-only declaration.

## The bound

`agents::code_agent::MAX_CONCURRENT_READ_ONLY_CALLS = 8`.

The width of a batch is the model's choice, not ours, so an unbounded fan-out
turns a forty-call batch into forty simultaneous file handles and forty whole
files resident at once. Eight is chosen against what these batches are actually
made of: the calls are io-bound, so the win is saturating one disk's queue depth
rather than matching a core count, and real batches are five to ten calls wide —
which eight collapses into a single wave. It is deliberately far below the
blocking pool `tokio::fs` dispatches onto; the scarce resource here is the
host's descriptors and the window's memory, not the runtime's threads.

`tests/parallel_readonly_batch.rs::concurrency_is_bounded` asserts the observed
peak is both *at most* and *exactly* eight for a 24-call batch — the first half
is the bound, the second half stops the test from passing because the code
accidentally serialised.

## Order is a property of the reporting, not of the racing

The trajectory written into the Session `ABox` is provenance; if completion
order leaked into it, every replay and every test that reads the `ABox` would
become flaky. It does not leak, because nothing observable is emitted from
inside a racing future:

- results land in a slot indexed by **request position**;
- the `Decision` records are reified up front, in request order, before anything
  runs;
- the panel's activity blocks are opened up front in request order and closed
  afterwards in request order;
- the `tool_result` blocks and the reified `ToolCall` nodes are written by one
  loop walking the batch in request order after the whole batch settles.

`results_and_trajectory_follow_request_order_not_completion_order` requests the
slowest call first, so a loop that reported by completion reverses it exactly.
`request_order_holds_across_repeated_batches` runs twenty rounds with rotating
delays, so no single round's ordering can be luck. Both were observed failing
against a deliberately completion-ordered variant before being trusted.

## Cancellation

The turn's `RunCancellation` token races the whole batch. Losing drops every
in-flight future at its own await point, so an interrupt leaves nothing running.

What each call did is still known: the slots filled before the drop hold real
results. The batch therefore reports exactly what completed and marks the rest
`interrupted`, and every `tool_use` still gets its pair — the model never sees
an unanswered call.

`interrupting_a_concurrent_batch_leaves_nothing_running` interrupts a five-call
batch after two have finished and three are parked, and asserts on the fixture's
own in-flight counter that it falls to zero, on the two real results surviving,
and on the three `interrupted` records.
