# Headless usage accounting

What a non-interactive (`--print`) run reports about what it spent, and how a
downstream consumer reads it without reverse-engineering the binary.

One shape carries it: **`polaris-code-run-usage/v1`**.

## The object

Every machine-readable output format emits this object, unchanged:

```json
{
  "schema": "polaris-code-run-usage/v1",
  "model_id": "anthropic/claude-sonnet-4-6",
  "input_tokens": 1234,
  "output_tokens": 56,
  "cache_read_tokens": 789,
  "cache_creation_tokens": 12,
  "total_tokens": 2091,
  "estimated_cost_usd": 0.004823700000000001,
  "pricing_basis": "anthropic-list-2026-09",
  "pricing": {
    "family": "sonnet",
    "input_per_mtok": 3.0,
    "output_per_mtok": 15.0,
    "cache_read_per_mtok": 0.3,
    "cache_write_per_mtok": 3.75
  }
}
```

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | Contract identity. **Check this first.** An unrecognised value means the shape moved; do not read the rest. |
| `model_id` | string | The model the run actually drove, read from the session's own `SessionSettings` after the agent resolved the call — not from the command line, not from an environment variable a consumer might read differently, and not the id a `/model` request asked for if the turn ran on another. It falls back to `AgentConfig` only for a session that never reached the agent, so an unused model is never reported as the served one. |
| `input_tokens` | integer | Billed full-price (uncached) prompt tokens. |
| `output_tokens` | integer | Billed completion tokens. |
| `cache_read_tokens` | integer | Billed prompt-cache hits, charged at the read tier. |
| `cache_creation_tokens` | integer | Billed prompt-cache writes, charged at the write tier. |
| `total_tokens` | integer | The sum of the four counters above. Always exactly their sum. |
| `estimated_cost_usd` | number or `null` | The four counters priced at `pricing`. `null` means **not priced**, never *free*. |
| `pricing_basis` | string | Stable identity of the price table consulted. Always present, including when `pricing` is `null`, so "unpriced" is attributable to a named table. |
| `pricing` | object or `null` | The rate card the cost was derived from, or `null` when the model family is unpriced. |

## Where to read it

| `--output-format` | Machine-readable? | Where the accounting is |
|---|---|---|
| `json` | yes — **authoritative** | the `usage` member of the single stdout object |
| `stream-json` | yes | the **final** stdout line: the one message whose `io_type` is `usage`, carrying this object serialized as its text |
| `text` | no | not reported — stdout is reserved for the assistant's answer. Use `json`. |

`json` and `stream-json` emit the identical object, so a consumer needs one
parser. A `stream-json` line is an ordinary `IOMessage`, so the accounting is
reached as:

```jsonc
// the last line of a `--output-format stream-json` run
{"content":{"Text":"{\"schema\":\"polaris-code-run-usage/v1\", …}"},
 "source":"System",
 "metadata":{"io_type":"usage"}}
```

Select on `metadata.io_type == "usage"`, then parse `content.Text` as the
object above. Exactly one such message is emitted per run, and it closes the
stream.

## What is *not* the accounting

Mid-stream messages with `io_type = "tokens"` are the conversation panel's live
meter: per-step **deltas**, `in` and `out` only, as strings, with no cache
tiers, no cost and no model identity. **Do not sum them.** They exist so a UI
can render a rising counter; they are not, and have never been, the billed
total. `docs/reference/guide.md` and the rustdoc on
`polaris_code::exec::USAGE_IO_TYPE` say the same.

The human-readable `Session usage` block that an *interactive* session prints
on exit (`TokenMeter::summary`) is a rendering for a person. Never parse it.

## The limit the run ran under

The `json` object also carries `max_output_tokens`, alongside `usage` rather
than inside it:

```json
{ "session_id": "…", "status": "completed", "usage": { }, "max_output_tokens": 16384 }
```

It is the `max_tokens` every request of the turn carried — the value
`AnthropicPlugin` was **registered** with, set by
`POLARIS_CODE_MAX_OUTPUT_TOKENS` and defaulting to `16384` (four times the
provider's own 4096). Configuration, not
accounting, which is why it is outside the versioned `polaris-code-run-usage`
record a consumer pins by schema, and why it is reported even when `usage` is
`null`.

Read it when you report a benchmark number. A response that reaches the limit
comes back truncated, and today's agent loop does not inspect the finish
reason, so a truncated answer ends the turn looking like a completed one. The
number is only interpretable next to the bound it was produced under.

An unset or malformed `POLARIS_CODE_MAX_OUTPUT_TOKENS` is not silently
defaulted: unset keeps `16384`, and anything that is not a positive integer is a
configuration error (`exit 2` for `--print`). Values above a model's ceiling are
passed through and rejected by the API, rather than clamped down to something
the operator did not ask for.

The interactive `Session usage` block prints the same limit as its last line.
It is a rendering for a person; never parse it.

## Why you can trust the numbers

The counters are the ones the run itself billed, not a reconstruction:

- The agent's `act` system folds each streamed usage event into the session's
  `TokenMeter` as the turn runs — the same meter, and the same increments, the
  live panel tally is fed from.
- At the end of the turn the run surface reads that meter **back out of the
  session context** (`TokenMeter::snapshot`) — the same resource `act` wrote
  to, addressed through the same context. Nothing is re-tokenized, re-counted
  from the transcript, or re-estimated after the fact.
- `total_tokens` and `estimated_cost_usd` are derived from one load of each
  counter, so the emitted total always equals the sum of the emitted parts and
  the emitted cost always prices exactly the emitted counts.
- `estimated_cost_usd` and the human `Session usage` summary go through one
  cost formula (`Price::cost_usd`), so they cannot disagree.

A consumer can re-derive the cost from `pricing` and the counters and get the
same number to floating-point tolerance. That is a consistency check, not a
second authority: the emitted value is the measurement.

`crates/polaris_code/tests/headless_usage_accounting.rs` proves all of the
above against a scripted model provider — no API key, no network, no spend.

The object above is verbatim output, not a sketch — including the cost's float
tail. `estimated_cost_usd` is a sum of four `f64` products, so it carries the
usual binary-floating-point residue (`0.004823700000000001` rather than a clean
`0.0048237`). Compare costs with a tolerance, or sum them and round once at the
end; do not test them for exact equality. The rate card itself *is* exact: the
rates are the decimals a rate card should show, and the drift guard asserts
that.

## Scope of the numbers

The counters are cumulative for the meter the run drove, read at the end of the
turn.

A `--print` run drives **exactly one turn in a fresh process with a fresh
meter**, so for a headless run this is precisely that run's total. An
interactive session shares one meter across every turn, so there the same
snapshot reads as "the session so far".

An **interrupted** run still reports what it spent before stopping. A turn cut
short is not a free turn.

A turn served by a selected topology reports on the same footing, through this
same object: same `schema`, same members, same `model_id`, read back from the
same session meter. There is no second shape of usage record and no
topology-specific accounting. The counters will usually be unmoved, because a
selected topology calls no model — and an unmoved meter is reported as unmoved,
not as absent. The run id is the executor's run id for the graph that ran, and
the turn number is the session's own reading, which a topology turn does not
advance because it runs outside `process_turn_with`.
`docs/topology-selection.md` covers what such a turn reports beyond the
accounting.

## `null` accounting

`usage` is `null` — and the `usage` key is still present — when the run had no
`TokenMeter` installed, or when the turn never addressed a session at all (for
example a `--session` id that does not exist, which fails before the executor
and exits `1`).

`null` means **unmeasured**. It does not mean zero, and a consumer must not
count it as a free run. The shipped binary always installs a meter, so a real
`--print` invocation against a reachable session reports an object.

## Pricing basis and drift

`pricing_basis` names the price table a cost was derived from, so evidence
recorded months ago still says which rates produced its number.

Rates live in `crates/polaris_code/src/usage.rs` (`price_for`). **Changing
any rate requires bumping `PRICE_BASIS` in the same commit**; the drift guard
`the_pricing_basis_and_its_rates_move_together` in
`tests/headless_usage_accounting.rs` pins the basis string and every rate
together, so a silent rate change fails CI rather than re-dating old evidence.

Unrecognised model families are deliberately unpriced rather than guessed:
`estimated_cost_usd` and `pricing` are `null`, and the counters still stand.

## Bounding what a run may spend

This document covers what a run **reports**. To make that number
**enforceable** — a limit the operator sets, which stops the run on a turn
boundary with its own exit code — see
[`docs/run-cost-ceiling.md`](run-cost-ceiling.md).

A ceiling reports itself as a separate `ceiling` member under its own schema,
`polaris-code-run-ceiling/v1`. **`polaris-code-run-usage/v1` is unchanged by
it**, set or not: what a run spent and what it was allowed are different facts,
and a consumer already reading `usage` needs no knowledge of ceilings. The
`null`-means-unpriced rule above holds there too — a money ceiling over an
unpriced model stops the run rather than becoming no ceiling at all.

## Versioning

`schema` changes when the shape changes incompatibly. A consumer pins the value
it was written against and refuses an unknown one, rather than reading fields
that may have been repurposed. Adding a field is not, by itself, an
incompatible change.
