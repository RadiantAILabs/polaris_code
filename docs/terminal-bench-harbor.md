# Terminal-Bench through Harbor

`evals/packs/terminal_bench_harbor/polaris_code.py` adapts the `polaris`
executable to Harbor's installed-agent interface. Harbor owns environments,
trials, log downloads, and verifiers. The adapter installs one executable and
runs one non-interactive turn in the task image's declared `WORKDIR`.

The adapter is separate from `evals/core` because Harbor supplies its own
lifecycle and result model. It is tested with Harbor 0.24.0.

## Installation

Pass exactly one of these Harbor agent keywords:

- `version=MAJOR.MINOR.PATCH`: install a pinned release from
  `https://github.com/RadiantAILabs/polaris_code/releases/download/vMAJOR.MINOR.PATCH/`.
  The adapter accepts the **sandbox's** Linux x86-64 CPU and downloads
  `polaris-code-<version>-x86_64-unknown-linux-musl.tar.gz`, verifies the
  accompanying `.tar.gz.sha256`, then extracts
  the `polaris` executable. Setup requires curl, tar, and sha256sum.
- `binary=/absolute/path/to/polaris`: upload a local executable built with
  `scripts/build-portable-binary.sh`. Only this artifact is copied; its SHA-256
  becomes the recorded agent version. This is useful for development.

Release installation uses a static Linux x86-64 musl executable on both
glibc and musl task images, matching the
[published target](publishing.md#ci-and-binary-releases). Other CPU types fail
with a message naming `x86_64-unknown-linux-musl`; use a compatible local binary
for development on other architectures. A release must contain the CLI flags
below; older executables cannot run this adapter's trajectory export.
The static archive has no glibc version or dynamic-loader dependency. Keep the
GNU archive for platforms that deliberately require that build (including
Homebrew); Harbor installs the musl archive so older task images work too.

## Model and permissions

Register `evals.packs.terminal_bench_harbor.polaris_code:PolarisCode` as a
custom Harbor agent. Put the repository root on `PYTHONPATH` when invoking Harbor, for example
`PYTHONPATH="$PWD" /path/to/venv/bin/harbor run --config job.yaml`
from the repository root. Pass the provider key through Harbor's agent environment. For a
Chat Completions endpoint, set `OPENAI_API_KEY` and `OPENAI_BASE_URL`, select
`provider=openai-chat`, and use Harbor model
`openai-chat/<remote-model-id>`. The application strips this prefix once;
slashes and provider suffixes inside the remote ID are preserved.

The adapter accepts these agent keywords in addition to `version` or `binary`:

| Argument | Meaning |
| --- | --- |
| `provider` | Optional explicit CLI provider, such as `openai-chat`. |
| `base_url` | Optional endpoint override, supplied as `OPENAI_BASE_URL`. |
| `reasoning_effort` | Native `--reasoning-effort`; name it explicitly for comparable evaluations. |
| `context_window` | Positive context-window override for an unfamiliar model. |
| `max_output_tokens` | Positive per-request ceiling; must be at least two tokens below `context_window`. |
| `unattended` | Explicit opt-in to `--dangerously-skip-permissions` inside a disposable environment; default `false`. |
| `allow_command` | Explicit shell-policy pattern or sequence of patterns when unattended mode is off. |
| `graph_search` | `curated`, `freeform`, or `both` (default). |

For example, a Harbor job config can name the custom agent as follows; replace
the binary path with your development build, and use a fresh job directory:

```yaml
agents:
  - import_path: evals.packs.terminal_bench_harbor.polaris_code:PolarisCode
    model_name: openai-chat/org/model:provider
    env:
      OPENAI_API_KEY: ${INFERENCE_API_KEY}
      OPENAI_BASE_URL: http://your-meter:8000/v1
    kwargs:
      binary: /absolute/path/to/polaris
      provider: openai-chat
      reasoning_effort: high
      context_window: 262144
      max_output_tokens: 32768
      unattended: true
```

For a released build, replace `binary` with `version` pinned to that release.
Polaris sends a fixed output ceiling on each request. For a strict route that
rejects input plus output allowance beyond the window, cap the output at
`min(model output limit, context_window // 4)`. Use that cap also when the
advertised output limit equals the entire context window, which leaves no
input room. The history budget is `floor(0.8 * (context_window - output cap))`;
cheap elision starts at half that budget and summarization at three quarters.
The key is never placed in an argument, generated configuration, or release
URL. Harbor scopes the environment to the agent. The adapter does not read a
host credential file or resolve keys from the model's provider prefix.

Without `unattended=true`, the adapter grants edits with `--allow-edit` and
passes only the explicitly supplied `--allow-command` patterns. Unattended
mode grants edits and shell syntax within the application's working-directory
boundary. Use it only in Harbor's disposable task environment.

## Logs and usage

The command uses `--output-format stream-json` and
`--trajectory-path <Harbor agent logs>/trajectory.json`. It also saves stdout
as `polaris.jsonl` and stderr as `polaris-stderr.txt`. The image's `WORKDIR`
is preserved; the adapter supplies neither a working-directory argument nor
an execution `cwd`. Shell quoting preserves the instruction as one argument,
and process failures propagate to Harbor.

After Harbor downloads the agent logs, `populate_context_post_run` validates
the native ATIF document and fills `AgentContext` with total prompt, completion,
and cached tokens, plus dollar cost when known. When ATIF has no price, the
adapter reads `estimated_cost_usd` from the application's authoritative
`polaris-code-run-usage/v1` system event in `polaris.jsonl`. Unknown pricing stays unset.
No trajectory converter is needed. Missing or invalid ATIF emits a warning and
leaves the context unchanged, preserving the original trial outcome.

Store Harbor job directories outside the repository: they contain task text,
model requests, command output, patches, and grader results. Each trajectory
destination must be fresh; the executable refuses to overwrite existing files.

## Adapter tests

The tests require Harbor but do not require a container, inference credentials,
or a model. They exercise the x86-64 archive path with a local release fixture,
unsupported-CPU and checksum rejection, CLI options, quoting, permissions, and native ATIF usage.

```bash
uv venv /tmp/polaris-harbor
uv pip install --python /tmp/polaris-harbor/bin/python 'harbor==0.24.0'
/tmp/polaris-harbor/bin/python -m unittest discover \
  -s evals/packs/terminal_bench_harbor/tests -t .
```
