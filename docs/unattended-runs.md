# Unattended model runs

An ordinary `polaris -p` run denies edits and shell commands unless the same
invocation grants them with `--allow-edit` and `--allow-command`. For isolated,
disposable containers, one explicit CLI flag grants edits and arbitrary shell
commands without prompting:

```sh
polaris /testbed -p 'Fix the failing tests' \
  --provider openai-chat \
  --model 'openai-chat/vendor/model:route' \
  --reasoning-effort high \
  --context-window 131072 --max-output-tokens 16384 \
  --dangerously-skip-permissions \
  --trajectory-path /logs/agent/trajectory.json \
  --output-format json
```

Set `OPENAI_API_KEY` and `OPENAI_BASE_URL` in the process environment. The URL
is the API root, such as `http://127.0.0.1:8080/v1`; Chat Completions requests go
to its `/chat/completions` route. The `openai-chat/` model prefix is optional
on `--model`. Only that prefix is removed; vendor slashes and suffixes are
sent verbatim. No built-in model name or extra catalog declaration is required.

## Container permissions

`--dangerously-skip-permissions` requires `-p` and cannot be enabled by an
environment variable, `.env`, settings, a profile or an in-session command.
It conflicts with the narrower `--allow-edit` and `--allow-command` flags.
It allows the normal file mutation tools and both foreground and background
shell execution. The executor's command deny list and conservative shell
syntax checks are bypassed through host-issued, invocation-bound receipts.
Arbitrary scripts, substitutions, loops and destructive commands can run.

**Use only inside a disposable container.** Shell commands execute with the
process's filesystem and network access; the working directory check is not
an operating-system security sandbox. Explicit permission-rule denials still
apply. File tools retain their working-directory and symlink checks; shell
working directories must remain inside the workspace. Edit journaling,
index refresh, result redaction, cancellation and review/rewind bookkeeping
use the same paths as ordinary grants. Other confirming tools still need
`--allow-tool`. The explicit container mode removes the normal 100-iteration
turn cap (the executor uses the largest representable iteration count). Set
`--max-turns` when an iteration limit is wanted; token/spend ceilings and
external timeouts still apply. Interactive sessions and ordinary `-p` runs keep their existing
permission behavior.

## Reasoning and model limits

`--reasoning-effort` pins effort at provider registration for every model call,
including retries, summaries, automatic reviews and topology calls. Its stable
environment and TOML settings are `POLARIS_CODE_REASONING_EFFORT` and
`reasoning_effort`. Known names are `none`, `minimal`, `low`, `medium`, `high`,
`xhigh`, and `max`; non-empty custom values pass to the framework. `none` is an
explicit value. Omitting the option leaves provider/model defaults in force.
Unsupported efforts fail visibly according to the provider's framework
contract, either before network I/O or as a server rejection. An auxiliary
call cannot silently choose a different effort. A set
`POLARIS_CODE_EXP_REASONING_EFFORT` now stops startup and names its replacement.

`--context-window` declares the model's total input-plus-output capacity;
`--max-output-tokens` declares its per-call output ceiling. Both accept positive
integers and have corresponding `POLARIS_CODE_*` environment variables and
snake-case TOML settings. The context window must exceed the output ceiling
by at least two tokens. If a model advertises an output cap equal to its context
window, choose a smaller run output allowance, for example a quarter of the
window. Polaris sends a fixed output ceiling, allowing auxiliary callers to
request less; it does not dynamically reduce output to remaining context.

The history projection budget is `floor(0.8 * (window - output))`; the other
20% of input capacity reserves system prompts, tool schemas and estimation
error. Cheap tool-output compaction begins at half that history budget, and
summary compaction at three quarters. Summary output is also bounded by the
run's ceiling. These are local token estimates, calibrated from provider
usage; no tokenizer can guarantee the exact boundary for arbitrary model ids.
The active user turn stays intact even when oversized. Without an explicit
window, existing retrieval defaults apply; Polaris does not infer an unknown
model's capacity from its name. The declared window applies throughout the
session, including after an interactive model change.

An unpriced model works with token limits; a dollar spend cap still requires
a known price. Unknown costs are never replaced with a fabricated zero price.

## Trajectory export

`--trajectory-path PATH` requires `-p`. Its parent directory must already
exist, and the destination must be fresh; existing files and symlinks are
refused before the first model call. The file is ATIF-v1.7 JSON, following
[Harbor's trajectory models](https://github.com/laude-institute/harbor/tree/main/src/harbor/models/trajectories).
On Unix it is created and replaced with mode `0600` because it contains task
content, complete model prompts and model outputs.

The initial user instruction is followed by one agent step per provider call,
including retries and auxiliary calls. Each step includes text, returned
reasoning, tool calls, linked observations and native token metrics. Failed
or interrupted calls remain visible. Every step's `extra.request` holds the
complete serialized framework request, including system prompt, projected
messages, tools, effort and output allowance. Non-streaming replies are in
`extra.response`; streaming replies retain every native framework event in
`extra.stream_events`, including reasoning signatures, identifiers and usage.
Tool observations retain the full native result in `extra.native_result`.
Tool outputs use the agent's normal redaction path. Provider credentials,
HTTP headers and provider configuration are never exported.

Atomic checkpoints occur before calls, at content-block boundaries, on usage
updates, on tool results and when a stream is dropped. Compaction cannot erase
recorded evidence. SIGINT/SIGTERM use normal cleanup and finalization. A hard
kill leaves the last complete checkpoint; a partially streamed block since
that checkpoint may be absent. `extra.status` remains `running` until the run
finalizes, and incomplete calls have their own status. A requested export
that fails to persist makes the process exit unsuccessfully.

`final_metrics` totals reported prompt tokens (uncached input plus cache reads
and cache creation), completion tokens and cache reads, without double-counting
usage updates within a call. Per-call `metrics.extra.native_usage` or native
stream events preserve reasoning-output and cache-creation counts. These totals
cover reported usage; interrupted calls without final usage can have unknown
billable tokens. Dollar estimates and the run's normal failure/limit reporting
remain in the headless JSON result. ATIF capture is independent of the selected
stdout format.
