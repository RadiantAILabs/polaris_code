# Run cost ceiling

How an operator bounds what one run may spend, and what happens when it gets
there.

[`docs/headless-usage-accounting.md`](headless-usage-accounting.md) is the
other half: that document says what a run *reports*, this one says how that
number is made *enforceable*. The two shapes are separate on purpose — see
[Why the ceiling is not part of `usage`](#why-the-ceiling-is-not-part-of-usage).

## Setting a limit

```bash
polaris -p "refactor the parser" --max-cost-usd 2.50
polaris -p "refactor the parser" --max-tokens 500000
polaris-code --max-cost-usd 5 --max-tokens 2000000     # both; either stops it
```

| Flag | Bounds | Enforceable on |
|---|---|---|
| `--max-cost-usd <USD>` | the run's estimated spend | priced models only — see [Unpriced models](#unpriced-models) |
| `--max-tokens <TOKENS>` | billed tokens, counting every tier (input, output, cache read, cache write) | any model |

Both flags work interactively and with `--print`; neither is required. A run
given neither has no limit, and reports `"ceiling": null` — which means *no
limit was set*, not *the limit was not reached*.

Both limits are **inclusive**: reaching one stops the run, because the next
turn would go past it. Either limit alone is enough to stop a run that has both.

A limit that cannot bound a run — zero, negative, not a number — is a
configuration error, reported on stderr with exit code `2` before anything
starts. It is never coerced to "no limit".

## Where the run stops

On a **turn boundary**, which is the only place either front end decides
whether to take another turn (`crates/polaris_code/src/run.rs`). The
ceiling is checked on both sides of that boundary and never inside a turn:

- **before** a turn starts — above the topology-selection point, so a run
  already at its limit starts nothing at all: no selection, no graph, no
  provider call; and
- **after** a turn completes, over the accounting that turn produced.

A turn already in flight is never aborted. The turn that crosses the line runs
to completion, its answer is reported, its work is kept, and the run ends
*after* it. Aborting mid-turn would lose that work and leave the session
inconsistent, which is the failure mode this design exists to avoid.

The session is saved exactly as for any other ending, before the process exits.

## Exit status

| Code | Meaning |
|---|---|
| `0` | the turn completed |
| `1` | the turn failed, or accepted work started no topology |
| `2` | usage or configuration error — including a limit that cannot bound a run, and a money limit on an unpriced model |
| **`3`** | **the run stopped because it reached its spend ceiling** |
| `130` | interrupted (`SIGINT`) |

`3` is distinct from `0` so an unattended caller can tell "stopped because it
ran out of budget" from "stopped because it was done" without parsing any
output, and distinct from `1` because reaching a limit the operator set is not
a failure. The constant is `polaris_code::run::CEILING_EXIT_CODE`.

## What the run reports

**stderr, every format** — one line naming both numbers:

```
polaris: spend ceiling reached, stopping: $2.5013 of $2.5000 allowed
```

**`--output-format json`** — the `ceiling` member, schema
`polaris-code-run-ceiling/v1`, beside `usage` rather than inside it:

```json
{
  "schema": "polaris-code-run-ceiling/v1",
  "max_cost_usd": 2.5,
  "max_tokens": null,
  "spent_usd": 2.5013,
  "spent_tokens": 934120,
  "stopped": true,
  "reason": "spent $2.5013 of the $2.5000 allowed"
}
```

| Field | Type | Meaning |
|---|---|---|
| `schema` | string | Contract identity. **Check this first**, as with `usage`. |
| `max_cost_usd` | number or `null` | The money limit the operator set; `null` when they set none. |
| `max_tokens` | integer or `null` | The token limit the operator set; `null` when they set none. |
| `spent_usd` | number or `null` | Estimated USD spent. `null` means **not priced**, never *free*. |
| `spent_tokens` | integer | Billed tokens spent, across every tier. |
| `stopped` | boolean | Whether the run ended here. |
| `reason` | string or `null` | Which limit stopped it — present exactly when `stopped` is. |

At least one of `max_cost_usd` / `max_tokens` is always present, because a run
with neither installs no ceiling and reports `"ceiling": null` for the whole
member.

`spent_usd` and `spent_tokens` are read off the same `RunUsage` the run
reports, so the two objects cannot disagree.

No new `stream-json` message type is introduced: a streaming consumer reads the
stderr line, or re-runs with `json`.

## Seeing it coming

An interactive session already shows the run's cost live in the status row.
With a ceiling set, the transcript also gets:

- a one-shot notice the first time the run passes **80%** of the tightest
  limit (`Approaching the spend ceiling: $2.0100 of $2.5000 allowed.`); and
- at the stop, an error-styled message plus a host-unavailable status, so the
  composer stops looking ready — this session is taking no more work.

On exit the interactive usage summary gains a `ceiling:` line stating the same
spent-against-allowed figures.

Library callers read `TurnOutcome::ceiling`, which is filled on **every**
outcome that has a ceiling, not only on the stop. `CeilingStatus::used_fraction`
is the fraction of the tightest limit used, which is what the 80% notice
watches.

## Unpriced models

`price_for` has no rate for every model, and a `null` cost means **not
priced**, never *free*. A money ceiling over a model with no rate therefore
cannot be shown to hold — so **the run refuses rather than proceeding
unbounded**. "Unpriced" never means "unlimited".

This is one rule (`RunCeiling::unenforceable_for`) applied at two points:

1. **At startup**, `polaris-code` refuses the combination outright and exits
   `2`:

   ```
   polaris: no price is known for model `some/unknown-model` under pricing
   basis `anthropic-list-2026-09`, so a $5.0000 spend ceiling cannot be
   enforced; bound the run in tokens instead
   ```

2. **At every turn boundary**, the same judgement runs again over the live
   accounting, so a run that reaches the loop anyway — a library caller, or a
   model that becomes unpriced mid-session — stops with exit `3` rather than
   spending on. Its `reason` names the model and the pricing basis, and
   `spent_usd` stays `null`.

Use `--max-tokens` to bound an unpriced model. Token counting needs no rate
card, so it works for every model, and a token-bounded unpriced run behaves
exactly like a priced one.

## Why the ceiling is not part of `usage`

What a run **spent** and what it was **allowed** are different facts with
different lifetimes. Adding a limit field to `polaris-code-run-usage/v1` would
change what that schema means for every consumer already parsing it — a
consumer that reads a cost would suddenly have to know whether a limit was in
play. So the ceiling gets its own member and its own version:
`polaris-code-run-ceiling/v1`. `usage` is byte-for-byte what it was before
ceilings existed, whether or not a limit is set.

## Scope

This is the **enforcement** half of durable usage accounting, over **one run**.
Durable, attributable accounting *across* sessions — a spend budget that
survives a process, attributable to a user or a project — is deliberately out
of scope and depends on where usage is stored. The meter a ceiling reads is the
one the run drove: a `--print` run gets a fresh process and a fresh meter, so
its ceiling bounds that invocation; an interactive session shares one meter
across its turns, so there the ceiling bounds the session.

## Proof

`crates/polaris_code/tests/run_cost_ceiling.rs` proves all of the above
against a scripted model provider — no API key, no network, no spend. The
scripted provider counts its own calls, so "the run took no further turn" is
asserted from that counter rather than inferred from an absent side effect.
