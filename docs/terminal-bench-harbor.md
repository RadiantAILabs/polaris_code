# Terminal-Bench through Harbor

Use this adapter to run Polaris on Terminal-Bench through Harbor, for example
for a leaderboard submission. `evals/packs/terminal_bench_harbor/polaris_code.py`
adapts the `polaris` executable to Harbor's installed-agent interface. Harbor
owns environments, trials, log downloads, and verifiers. The adapter installs
one executable and runs one non-interactive turn in the task image's declared
`WORKDIR`.

The adapter is separate from `evals/core` because Harbor supplies its own
lifecycle and result model. It is tested with Harbor 0.24.0.

## Installation

Pass exactly one of these Harbor agent keywords:

- `version=MAJOR.MINOR.PATCH`: install a pinned release from
  `https://github.com/RadiantAILabs/polaris_code/releases/download/vMAJOR.MINOR.PATCH/`.
  A leading `v` and a pre-release suffix such as `0.3.0-rc.1` are accepted. The
  sandbox must be Linux on an x86-64 CPU. The adapter downloads
  `polaris-code-<version>-x86_64-unknown-linux-musl.tar.gz`, checks it against
  the `.tar.gz.sha256` file of the same release (that file must name the
  archive), then extracts the `polaris` executable. Setup installs `curl` and
  `tar` when the image lacks them. The image must already provide `sha256sum`.
  The checksum file comes from the same release as the archive, so it detects
  corruption but not a replaced release. For a reproducible run, also pass
  `sha256=<64 hex digits>`, the digest of the archive: the script then compares
  that digest and does not fetch the checksum file. Record the digest with the
  results. `sha256` cannot be combined with `binary`.
- `binary=/absolute/path/to/polaris`: upload a local executable built with
  `scripts/build-portable-binary.sh`. Only this artifact is copied; its SHA-256
  becomes the recorded agent version. This is useful for development.

Release installation uses a static Linux x86-64 musl executable on both
glibc and musl task images, matching the
[published target](publishing.md#ci-and-binary-releases). Other CPU types fail
with a message naming `x86_64-unknown-linux-musl`; use a compatible local binary
for development on other architectures. A release must contain the musl archive
and the CLI options this adapter passes (`--trajectory-path`,
`--dangerously-skip-permissions`, `--reasoning-effort`, `--context-window`,
`--max-output-tokens` and `--provider`); older executables cannot run this
adapter's trajectory export.
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
| `base_url` | Optional endpoint override, supplied as `OPENAI_BASE_URL`. If the agent environment (`env:` in the job config) also sets `OPENAI_BASE_URL`, Harbor uses that value instead. |
| `reasoning_effort` | Native `--reasoning-effort`; name it explicitly for comparable evaluations. |
| `context_window` | Positive context-window override (up to 4294967295) for an unfamiliar model. |
| `max_output_tokens` | Positive per-request ceiling (up to 4294967295); must be at least two tokens below `context_window`. |
| `unattended` | Explicit opt-in to `--dangerously-skip-permissions` inside a disposable environment; default `false`. Not allowed together with `allow_command`. |
| `allow_command` | Explicit shell-policy pattern or sequence of patterns. Refused when `unattended` is true, which already allows every command. |
| `sha256` | Optional SHA-256 of the release archive, with `version`; see [Installation](#installation). |
| `graph_search` | `curated`, `freeform`, or `both` (default). |

For example, the `agents` entry of a Harbor job config (dataset and task
settings are omitted) can name the custom agent as follows; replace the binary
path with your development build, and use a fresh job directory:

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
passes only the explicitly supplied `--allow-command` patterns. With
`unattended=true` it passes `--dangerously-skip-permissions`. The model may then
edit any file the process can reach, including paths outside the working
directory, and run any shell command: the command deny list and syntax checks
are off, and the working-directory check is not an operating-system sandbox. The
image's `WORKDIR` is only where the run starts. **The Harbor task container is
the only boundary**, so use unattended mode only there. It also removes the
normal 100-iteration turn cap, and the adapter has no `max_turns` keyword, so
spend is limited only by token and cost limits and Harbor's timeouts.

In unattended mode, commands the model writes run as the same user as `polaris`,
and can read the provider key (for example `OPENAI_API_KEY`) from its
environment. Use a scoped, metered, revocable key for these runs and revoke it
afterwards.

## Logs and usage

The command uses `--output-format stream-json` and
`--trajectory-path <Harbor agent logs>/trajectory.json`. It also saves stdout
as `polaris.jsonl` and stderr as `polaris-stderr.txt`. The image's `WORKDIR`
is preserved; the adapter supplies neither a working-directory argument nor
an execution `cwd`. Shell quoting preserves the instruction as one argument
(passed as `--print=<instruction>`, so an instruction that starts with `-` is
still a value), and process failures propagate to Harbor.

After Harbor downloads the agent logs, `populate_context_post_run` validates
the native ATIF document and fills `AgentContext` with total prompt, completion,
and cached tokens, plus dollar cost when known. When ATIF has no price, the
adapter reads `estimated_cost_usd` from the application's authoritative
`polaris-code-run-usage/v1` system event in `polaris.jsonl` (see
[headless usage accounting](headless-usage-accounting.md)). It reads only the
last 8 MiB of that file, skips lines it cannot decode or parse, and uses the
last valid event. Unknown pricing stays unset. The sandbox writes these files,
so token and cost figures are not audited: a model with unattended access can
change them. No trajectory converter is needed. Missing or invalid ATIF emits a
warning and leaves the context unchanged, preserving the original trial outcome.

Store Harbor job directories outside the repository: they contain task text,
model requests, command output, patches, and grader results. Each trajectory
destination must be fresh; the executable refuses to overwrite existing files.

## Adapter tests

The tests require Harbor but do not require a container, inference credentials,
or a model. They exercise the x86-64 archive path with a local release fixture,
unsupported-system, checksum and digest-pin rejection, CLI options, quoting,
permissions, and native ATIF and stream-log usage. They also run the generated
agent command against a stub executable to check the log files and exit status.

Without Harbor the tests are skipped; set `POLARIS_REQUIRE_HARBOR=1` to make a
missing Harbor an error. The `Harbor adapter tests` job in CI sets it, installs
Harbor 0.24.0 on Python 3.12, and is part of the required `CI` check.

```bash
uv venv /tmp/polaris-harbor
uv pip install --python /tmp/polaris-harbor/bin/python 'harbor==0.24.0'
/tmp/polaris-harbor/bin/python -m unittest discover \
  -s evals/packs/terminal_bench_harbor/tests -t .
```
