# Harness evaluation

The Python code under `evals/` evaluates the shipped `polaris-code` binary from
outside the Rust crate. It runs each task in a disposable environment, captures
the patch and the binary's own usage record, invokes the dataset's grader, and
derives an immutable summary. Evaluation outputs, logs, transcripts, and score
tables belong outside this repository.

## Core and dataset packs

The dependency points in one direction: a dataset pack imports the core, while
the core never names a dataset.

`evals/core/` owns run identity, immutable manifests, output digests, usage
accounting, budget enforcement, episode scheduling, statistics, replay, and
summary generation.

`evals/packs/<name>/` owns the dataset-specific suite, split policy,
environment, grader, pin checks, and command-line additions. A pack implements
`evals.core.pack.DatasetPack` and registers itself in `evals/packs/__init__.py`.

The repository includes:

- `swebench_verified`, the SWE-bench Verified comparison pack;
- `swebench_multilingual`, a pinned multilingual suite;
- `terminal_bench_harbor`, a Harbor adapter described in
  [`terminal-bench-harbor.md`](terminal-bench-harbor.md).

The external baselines in `evals/external/` run stock Claude Code or Codex
against the same task inputs without importing either agent.

## SWE-bench Verified

`evals.packs.swebench_verified` pins the public dataset rows and official
SWE-bench grader inputs in `evals/packs/swebench_verified/suite/`. Each row
retains its repository, base commit, version, and official image. The pack
resets every disposable checkout to that base before starting the agent.

The default API spend ceiling is zero. A live run therefore requires an
explicit nonzero `--api-cost-ceiling-usd`; this prevents an accidental paid
sweep. Preflight, pin verification, and the gold-patch grader smoke do not run
the agent:

```bash
python3 -m evals.packs.swebench_verified pin
python3 -m evals.packs.swebench_verified preflight \
  --container-runtime docker \
  --grader-python python3
python3 -m evals.packs.swebench_verified gold \
  --output /tmp/swebench-verified-gold \
  --grader-python python3
```

Build the portable agent before a containerized run:

```bash
scripts/build-portable-binary.sh
```

The build helper produces a static musl binary at
`target/x86_64-unknown-linux-musl/release/polaris-code`, which is the default
path used by the pack.

Run `python3 -m evals.packs.swebench_verified run --help` for the full command.
A live run has this general shape:

```bash
python3 -m evals.packs.swebench_verified run \
  --output /tmp/polaris-swebench-run \
  --model aws.bedrock/MODEL_ID \
  --container-runtime docker \
  --grader-python python3 \
  --allow-edit \
  --api-cost-ceiling-usd LIMIT
```

Provider credentials are inherited by the agent process and are never written
to the manifest. Use `--allow-command` for each shell-policy pattern the run is
allowed to execute. Transcripts are discarded unless `--keep-transcripts` is
set; retained transcripts contain problem text and should be treated as run
artifacts, not source files.

## Experiment axes

The harness can vary one recorded axis while holding the others fixed:

- `graph-search` compares the curated, freeform, and combined code-graph
  surfaces;
- `topology` compares named registered topologies through `--topology-work`;
- `composition` compares deterministic, model-selected, and gate-planned
  composition policies.

Subsets and replay scripts are plumbing checks, not scored measurements. Their
records keep the full suite denominator and are labelled accordingly. Complete
conclusions require the pack's configured repetition count and statistical
evidence bar; missing episodes never shrink the denominator.

## Outputs and replay

A run creates a new output directory and never mutates a completed one. The
manifest records the binary digest, dataset pins, model and policy settings,
permission grants, scheduling seeds, and budget. Episode records carry the
prediction patch, grader disposition, and versioned usage object.

`recompute` regenerates derived summaries from immutable episode records and
checks their digests. `--replay` exercises the same plumbing with recorded
scripts without calling a model; replay output is always labelled as a
demonstration rather than benchmark evidence.

## Development checks

The harness tests are deterministic and do not call a model:

```bash
python3 -m unittest discover -s evals/core/tests -t .
python3 -m unittest discover -s evals/packs/swebench_verified/tests -t .
python3 -m unittest discover -s evals/packs/swebench_multilingual/tests -t .
```

The Rust workspace test suite also invokes the external adapter tests.
