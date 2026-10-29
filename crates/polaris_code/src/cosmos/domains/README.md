# `cosmos/domains/` — domain plans: specialised versus generic

A domain plan carries per-repository notes (and, optionally, extra gate blocks and a plan shape)
for one project, so a run can compare a generic statement-only plan against specialised ones.
Which plan a run uses is chosen by `POLARIS_CODE_DOMAIN` and `POLARIS_CODE_ARM` (see
`polaris --help`). This file is the "how to add a domain plan" note.

## Layout

```
domains/
  mod.rs              Domain, Arm, DomainSelection; plan_for_selection dispatch.
  README.md           this file.
  <domain>/mod.rs      re-exports the variant modules.
  <domain>/expert/     `expert` arm: researched notes and plan.
    mod.rs               NOTES, plan(), blocks module
    blocks.rs            domain-specific expert gate blocks
    notes.md             the domain's expert notes (≤1,200 words)
  <domain>/experience/ `experience` arm: domain-specific notes and plan.
    mod.rs               NOTES, plan(), blocks module
    notes.md             the domain's experience notes (≤1,200 words)
  <domain>/expert2/    `expert2` arm: staged expert plan.
    mod.rs               plan(), blocks module
```

`<domain>` is `django`, `sympy`, or `sphinx`. A change to one variant should stay inside that
`<domain>/<variant>/` directory — not `mod.rs` at any level above it, not another domain's files,
and not `planner.rs` or `host.rs`.

## What a new variant starts as, and why

A new variant starts identical to the generic (`Arm::Generic`) plan:

- `notes.md` starts empty (0 bytes). It is read at build time via `include_str!`, so an edit to it
  needs a rebuild, not a restart.
- `plan()` starts as `planner::statement_only_plan_body(facts, attempt, NOTES)` — the exact same
  statement-only shape the generic arm uses, with this variant's own (possibly empty) notes
  threaded through. A test in `domains/mod.rs` (`every_domain_variant_builds`) checks that every
  variant builds a plan; only the generic arm is pinned to the generic digest.
- `blocks::keys()`/`blocks::register_all()` start empty. They are already wired into the sealed
  coding block catalogue (`gate_blocks::coding_block_catalogue` calls
  `domains::register_all`), so adding a real block later needs no edit outside this file.

## What a plan function receives

```rust
pub fn plan(
    facts: &crate::cosmos::planner::PlannerFacts,
    attempt: polaris_cosmos::composition::RevisionAttempt,
) -> crate::cosmos::planner::StatementPlanBody;
```

`facts` is the same typed view of the turn the generic scripted planner reads — see
`PlannerFacts`'s own field docs in `planner.rs` (`registration`, `scope_size`, `success_check`,
`objective`, `test_runner_available`, `shell_available`, `statement`, …). `attempt` is this gate
plan's one-based revision-loop position; only `test_gated_expression`/`model_test_gated_expression`
degrade their shape by it today, and a domain plan is free to ignore it. The return type is the
statement-only branch's own `(GateExpr, extra per-instance configurations)` pair — the same shape
`planner::statement_only_plan_body` returns and `planner::scripted_plan_generic` merges into the
whole plan's `configurations` map. Build a `GateExpr` from `polaris_cosmos::composition`'s grammar
(`Seq`, `Choose`, `Switch`, `Repeat`/`Loop`, `Guard`, `Fork`) over block keys from the sealed
coding catalogue (`crate::cosmos::gate_blocks::{existing, configurable, repair, survey,
clarification}`, plus this variant's own `blocks` module once it registers one) — see
`docs/gate-planning.md`'s "Block descriptions" section for what
is already available before reaching for a new one.

To add a domain-specific block: write it in this variant's own `blocks` module following the
pattern any `gate_blocks` submodule already uses (a `BlockFactory` impl, a stable
`CompositionBlockKey`, a configuration schema — `no_config_schema()` if it needs none), add its key
to `blocks::keys()`, and register it in `blocks::register_all()`. It becomes nameable in this
variant's own `plan()` without touching `gate_blocks/mod.rs` or any other domain's files.

## The `generic-notes` and `expert` arms must carry byte-identical notes text

`planner::statement_only_plan_body(facts, attempt, notes)` is the one place `notes` reaches a
model step's instruction. `Arm::GenericNotes` (in `domains::plan_for_selection`) calls it
directly with the selected domain's `expert::NOTES`; a variant's own `expert` `plan()` calls the same
function with the same constant. Never copy or reformat the notes text into a second place — read
`expert::NOTES` (or thread it in as a parameter) so a change to `notes.md` reaches both arms at
once and a diff between them is only ever the plan's structure, never the words.

## How to test a change here

1. Add or extend a test in this variant's own `mod.rs` (or a new `#[cfg(test)] mod tests` there)
   that builds `PlannerFacts` for a representative statement-only task (see `statement_only_facts`
   fixtures in `planner.rs`'s and `domains/mod.rs`'s own test modules for the shape), calls
   `plan()`, and asserts on the resulting `GateExpr` — e.g. that it names your new block, or that
   `polaris_cosmos::composition::materialize` accepts it against
   `gate_blocks::coding_block_catalogue()` and `gate_blocks::coding_target_contracts()`.
2. If you added a block, no extra catalog test is needed:
   `domains::keys()` feeds
   `crate::cosmos::gate_blocks::tests::the_real_catalogue_seals_with_every_registration_distinct`, so a key your
   `blocks::keys()` returns but `blocks::register_all()` never registers fails it immediately.
3. Run `cargo +1.97.0 test --workspace`, `cargo +1.97.0 fmt -- --check` and
   `cargo +1.97.0 clippy --all-targets --all-features -- -D warnings` before committing.
4. Do not open the upstream issue or pull request of any SWE-bench Verified task while researching
   notes: notes written from a task's own fix would leak the answer into the benchmark.
