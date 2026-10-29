# Terminal-Bench through Harbor

`evals/packs/terminal_bench_harbor/polaris_code.py` adapts the shipped
`polaris-code` binary to Harbor's installed-agent interface. Harbor owns the
task environments, trials, logs, and verifiers; the adapter only installs the
binary and runs one non-interactive turn in the task container.

This adapter is separate from `evals/core` because Harbor supplies its own
evaluation lifecycle and result model.

## Prerequisites

- a Harbor installation compatible with the adapter imports;
- a portable `polaris-code` binary built with
  `scripts/build-portable-binary.sh`;
- provider credentials available to Harbor's agent environment;
- the Terminal-Bench dataset configured in Harbor.

The adapter receives the host binary through Harbor's `binary` agent keyword,
uploads only that artifact, makes it executable, and records its SHA-256 as the
agent version. It does not copy the source tree or read provider credentials.

## Running

Register `PolarisCode` as a Harbor installed agent, then pass the binary as an
agent keyword. The Harbor CLI syntax depends on the installed Harbor release;
the important adapter arguments are:

| Argument | Meaning |
| --- | --- |
| `binary` | Absolute path to the portable `polaris-code` executable. |
| `allow_command` | Optional shell-policy pattern; repeat through Harbor when more than one is required. |
| `graph_search` | `curated`, `freeform`, or `both` (default). |

Harbor must also supply a model name. The adapter invokes `polaris-code -p`
with `--output-format stream-json`, `--allow-edit`, and any explicit command
grants. It deliberately leaves the working directory unset so each task
image's declared `WORKDIR` remains the containment boundary.

Store Harbor trial directories outside the repository. They can contain task
text, command output, model transcripts, patches, and grader results.

## Adapter tests

The adapter tests use stubs and do not require a live model:

```bash
python3 -m unittest discover \
  -s evals/packs/terminal_bench_harbor/tests -t .
```
