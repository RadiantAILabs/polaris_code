# Evaluation harness

Everything under `evals/` runs outside the shipped agent and launches the
`polaris-code` binary as a subprocess.

| Path | Holds |
|---|---|
| `core/` | Dataset-neutral evaluation: run identity, accounting, budget guard, statistics, reports and the command line |
| `packs/<name>/` | One dataset each: suite, environment, grader and policy |
| `external/` | Adapters that run other agents on the same instances |
| `reproduce/` | Scripts and notes for reproducing a published comparison |
| `dynamic_topology/`, `tasks/` | The topology fixture suite described below |

## Dynamic-topology fixtures

A small, mostly-free measuring stick for one claim:
**the polaris-code agent composes a different graph for a different task.**

This is deliberately not `evals/core`/`evals/packs`: those measure whether the
shipped agent *resolves* SWE-bench-shaped problems, against an official
grader, at real cost. This suite measures something narrower — for a handful
of small, self-contained task fixtures, does the binary's own
`--output-format json` report show a *different, real, validated Polaris
graph* running for a *different task shape*, and did the work actually land.
Both questions are checked without a model wherever the task shape allows it.

## Layout

```
evals/tasks/<fixture>/
  task.md        # the shape this fixture probes, how it is submitted, and why
  expect.toml    # machine-checkable route, outcome and expected plan
  workspace/     # the tiny real project, as the agent would first see it
  solved/        # the same project already fixed, for testing the checker itself
evals/dynamic_topology/
  expect.py      # loads and validates expect.toml
  spec.py        # builds the --topology-task spec / --print argv for a route
  runner.py      # copies workspace/ to a temp dir, runs one turn, collects evidence
  checks.py      # checks the outcome and the chosen plan against expect.toml
  report.py      # renders the results table
  cli.py         # `python3 -m evals.dynamic_topology run ...`
  tests/         # deterministic tests of the above — no model, no binary
evals/run.sh     # thin wrapper around the CLI
```

## Running it

Build a binary once:

```bash
cargo build --bin polaris-code
```

Then:

```bash
bash evals/run.sh
```

This runs every fixture whose route needs no model against
`target/debug/polaris-code` (or `$POLARIS_CODE_BIN`, or `--binary PATH`), with
a deliberately invalid `ANTHROPIC_API_KEY` placeholder if the environment has
none (see "Cost", below), and writes a report under
`target/eval-reports/dynamic-topology/`. Exit code is `0` unless a run fixture
failed its outcome or its plan check; a skipped fixture never fails the run.

```bash
bash evals/run.sh py-oneline-fix rs-read-only-question   # only these two
bash evals/run.sh --composition-policy model              # spends money, see below
bash evals/run.sh --include-conversational                # spends money, needs a real key
bash evals/run.sh --keep-tmp                               # leaves each fixture's temp copy on disk
```

## Cost

Every fixture's route is one of:

| Route | Needs a model? | Why |
|---|---|---|
| `topology-edit` / `topology-statement`, `deterministic` policy | No | `cosmos::composition::deterministic_pick` (`crates/polaris_code_core/src/cosmos/composition.rs`) never calls a model, and none of the five registrations this build admits with no test runner and no model provider wired (`code.direct-edit`, `code.prediction-error-edit`, `code.worklist-sweep-edit`, `code.tree-question`, `code.coverage-audit`) call one inside their own graph either. |
| `topology-edit` / `topology-statement`, `model` policy | Yes, once per fixture | `composition::choose`'s `Model` arm makes one real, billed call to pick the recipe (`composition.rs`), even though the graph itself still does not call a model. |
| `conversational` | Yes | The ordinary agent loop decides what to do and how, which is a model call by construction. |

`polaris-code` itself refuses to start at all without `ANTHROPIC_API_KEY` set
(`crates/polaris_code_core/src/main.rs`), regardless of whether the turn it is
about to run needs a model — so a `topology-*`/`deterministic` fixture still
needs the variable *present*, just not valid. `runner.py` supplies the
deliberately invalid placeholder `not-a-real-key` whenever the environment has
none, for exactly the
fixtures that are documented above to never reach a provider call. A
`conversational` fixture's `expect.toml` sets `requires_live_model = true`;
`run.sh` skips it by default and only attempts it under `--include-conversational`
with a real key already in the environment — never with the placeholder, and
never inside this suite's own deterministic tests (`evals/dynamic_topology/tests/`
never starts the binary or sets a real key).

## What each fixture probes

| fixture | task shape | route | what a sensible plan looks like |
|---|---|---|---|
| `py-oneline-fix` | one-line fix (Python) | `topology-edit`, `intent=fast budget-ms=1000` | `code.direct-edit` / leanest recipe, two blocks, no loop |
| `rs-careful-verify-edit` | same edit, `intent=careful` | `topology-edit` | `code.prediction-error-edit` / predict-verify-compare loop |
| `rs-repair-loop` | bug + failing test (repair loop) | conversational | none — stated in prose, not directed at a topology |
| `rs-refactor-rename` | refactor/rename across files | conversational | none — the agent finds the sites, a spec states them up front |
| `rs-add-test` | add a test | conversational | none — stated in prose, not directed at a topology |
| `rs-read-only-question` | read-only question | `topology-statement`, `intent=answer` | `code.tree-question` / read-only, no loop |
| `rs-survey-coverage-audit` | survey-then-answer over many files | conversational | none — stated in prose, not directed at `code.coverage-audit` |
| `rs-ambiguous-kind` | ambiguous kind | conversational | none — matches no CLI or auto-route shape |

Every "none" above is deliberate: the conversational fixtures state the task
in prose, and each fixture's own `task.md` says how it is submitted. This build's headless entry points are
`--topology-task`'s two spec forms and `cosmos::host::auto_route`'s lexical
prefixes. Several conversational fixtures are natural candidates for the
planner-driven `gates` composition policy, which the runner exposes through
`--composition-policy gates`.

`code.worklist-sweep-edit` is the one hermetically-admissible registration no
fixture here submits a task to directly; its selection at cold start under a
`Thorough` intent is instead cited from an existing test
(`tests/topology_selection_learns_from_outcomes.rs`,
`an_empty_record_leaves_the_cold_start_exactly_as_it_was`) in
`rs-careful-verify-edit/task.md`, alongside the `Fast`/`Careful` mappings that
fixture and `py-oneline-fix` actually exercise.

## The deterministic test

`evals/dynamic_topology/tests/` never starts `polaris-code` and never calls a
model:

- `test_spec.py` proves the `--topology-task` strings this suite builds
  round-trip through a literal, cited port of the real parser
  (`terminal::topology::split_fields`/`read_value`).
- `test_checks.py` proves the outcome checker itself: every `command`-checked
  fixture's checker is run for real (`cargo test` / `python3 -m unittest`)
  against both its checked-in `workspace/` (must fail) and `solved/` (must
  pass) copies, and every `answer_contains`/plan check is run against small,
  hand-built report dictionaries shaped like `RunReport`'s JSON, including
  reports deliberately built wrong, so a checker that stopped checking
  something would fail one of these before it could pass a fixture it should
  not have.

Run it with:

```bash
python3 -m unittest discover -s evals/dynamic_topology/tests -t .
```

`cargo test --workspace` already runs it too, with no change needed there:
`crates/polaris_code_core/tests/harness_eval.rs`'s
`workspace_tests_execute_the_external_adapter_suite` discovers every test
under `evals/`, this package included.
