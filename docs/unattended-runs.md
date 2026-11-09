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
It allows file tools to reach any path accessible to the process, including
absolute paths, parent traversal, and symlinks outside the working directory.
Relative paths still start in the working directory. It also allows both foreground and background
shell execution. The executor's command deny list and conservative shell
syntax checks are bypassed through host-issued, invocation-bound receipts.
Arbitrary scripts, substitutions, loops and destructive commands can run.

**Use only inside a disposable container.** Shell commands execute with the
process's filesystem and network access; the working directory check is not
an operating-system security sandbox. Explicit permission-rule denials still
apply when their selectors match. File-tool containment is disabled only for
this flag; exclusive file creation and normal result redaction remain in force.
Shell working directories still start inside the workspace, while commands may
touch arbitrary paths. Edit journaling,
index refresh, result redaction, cancellation and review/rewind bookkeeping
use the same paths as ordinary grants. External edits are journaled; workspace
indexing and safe rewind remain confined to workspace files. Other confirming tools still need
`--allow-tool`. The explicit container mode removes the normal 100-iteration
turn cap (the executor uses the largest representable iteration count). Set
`--max-turns N` when an iteration limit is wanted. `--max-turns 0` explicitly
disables the cap in any mode; the environment and TOML equivalents are
`POLARIS_CODE_MAX_TURNS=0` and `max_turns = 0`. Token/spend ceilings and
external timeouts still apply. Interactive sessions and ordinary `-p` runs keep their existing
permission behavior.

## When a run ends

A `-p` run ends on a model reply that calls no tool, with two exceptions:

- **A reply cut off at the output limit is continued.** If the provider stops
  the reply because it reached `--max-output-tokens`, the agent asks the model
  to continue from where it stopped, up to three times per turn. This applies
  to interactive sessions too.
- **An unattended reply that calls no tool can be asked to go on.** Nobody can
  answer a `-p` run, so a reply that only describes its next step would end the
  run with the work undone. `POLARIS_CODE_UNATTENDED_NUDGES` sets how many times
  a turn sends a message saying the run is unattended and asking the model to
  keep working. It is `0` (off) by default: on benchmark runs, every such
  message came after the work was finished. A value that is not a non-negative
  integer is a configuration error (exit `2`). Interactive sessions never send them, and
  neither does a host that embeds the agent unless it sets
  `ContinuationPolicy::unattended_nudges` itself.

Both messages start with `[continuation] `, appear in the trajectory as user
messages, and are recorded as `continuation` decisions. Every budget is finite,
so a model that keeps replying without tools still ends the run. Only the
output limit counts as a cut-off: a reply the provider stops with `pause_turn`,
or because of a content filter, is not continued as one. In an unattended run
it is handled like any other reply without a tool call, so it uses the nudge
budget; in an interactive session it ends the turn.

A `-p` run whose prompt starts like a question (`find …`, `where is …`,
`show me …`) is answered by the read-only question topology only when the run
grants neither edits nor commands. A run that grants either always runs the
full agent, because the question topology cannot change the workspace.

A failed model call ends the run, so a `-p` run retries a network error,
timeout, rate limit or overload for about five minutes: ten attempts, with
waits starting at 2 s and doubling up to 60 s. A call that times out on every
attempt can take up to 59 minutes. Interactive sessions keep five
attempts, about fifteen seconds of waiting, because a person can send the
message again. An authentication or invalid-request error is never retried.
A reply that cannot be read, such as a tool call whose arguments are not valid
JSON, is sent again up to two more times in every front end, even if it had
already shown text: nothing of it can be kept. Each attempt is billed.

## Shell commands

Foreground `run_command` calls in a `-p` run time out after 600 seconds unless
the call sets its own `timeout_secs`; interactive sessions keep 120 seconds,
because a person can send a long run to the background there. A command the
model starts with `run_in_background` keeps its own default of 3,600 seconds in
every mode.

The model sees at most 10,000 characters of each output stream: the first and
the last 5,000, with a line saying how much was left out. When a stream is
longer than the shell's 64 KB capture, the end comes from the overflow file
that holds the whole stream, and the result names that file. The trajectory
keeps the executor's full record, and the terminal shows the same text the
model sees.

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
Replayed assistant reasoning counts toward these estimates and compaction
thresholds. Elision, interrupted-tool repair and summary projection preserve
reasoning on every retained assistant message; summaries replace complete older
spans. See [context compaction](context-compaction.md) for provider differences.
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

Each new user or assistant message has one ATIF step, including new compacted
summaries and elided versions of tool results. System prompts, tool definitions
and request parameters are interned as system steps before the first call that
uses them. Unchanged configuration is reused. A new version of a prompt, tool
palette or schema is stored once when an auxiliary call needs it.

Each inference has an agent step with `llm_call_count: 1`, its UTC ISO-8601
start timestamp, returned text/reasoning, tool calls, native usage, stop reason,
response identifiers and requested effort. Tool results follow ATIF's same-step
observation contract: text is stored once beside the tool call it answers.
The corresponding result-message step references that observation. Copied
assistant context is marked `is_copied_context`; it is not a new inference.
Token-level stream events and full repeated requests/responses are omitted.
Partial tool arguments and reasoning signatures remain available.

A call's `extra.input` contains one-based configuration step ids (`system`,
`tools`, `parameters`) and a `history` id. Null system/tools ids mean those
request fields were absent. Zero means an empty history. The system prompt is
its step's `message`. A tools step's `extra.value` lists step ids containing
individual definitions in their `extra.value`. The parameters step's
`extra.value` maps native request field names to value-step ids (or null for
null values). Individual schemas, definitions and parameter values are thus
shared even when a palette or cache breakpoint changes. Effort is on the
call's standard `reasoning_effort` field.

Histories are stored in `extra.histories` (one-based ids). Each has a `length`
and ordered `segments`: `{steps: [step_id, ...]}` supplies messages directly;
`{history: earlier_id, start: zero_based_offset, length: count}` reuses an
unchanged range. Normal appends, sliding windows and summary compaction share
old ranges instead of copying all preceding step indexes for every call.
Thus a long append-only run grows with its new content and per-call metadata.

Message steps retain a native message skeleton in `extra.native_message`.
`extra.native_refs` fills its null content fields from JSON pointers into the
ATIF document (`native` identifies the skeleton field; `value` identifies its
stored value). Optional `start`/`end` are UTF-8 byte offsets for reasoning
fragments. This retains exact native block order, signatures, tool ids and
media metadata without duplicating visible text. Non-object tool arguments
use `arguments._native_arguments` so the ATIF object remains valid.

`polaris_code::trajectory::rebuild_request(document, step_id)` reconstructs an
exact native request from these references. The exporter regression tests
round-trip requests before and after compaction and check file-size growth
over 128 calls. Compaction preserves the original observations alongside new
summary/stub messages. Tool outputs use normal redaction. Provider credentials,
HTTP headers and provider configuration are never exported.

Atomic checkpoints occur before calls, at content-block boundaries, on usage
updates and when the run is finalized. Tool results, messages and a dropped
stream's partial content are kept in memory and the next checkpoint writes
them. A response that already failed keeps the `failed` status. Compaction
cannot erase recorded evidence. SIGINT/SIGTERM use normal cleanup and
finalization. A hard kill leaves the last complete checkpoint; a partially
streamed block, tool results or messages since that checkpoint may be absent.
`extra.status` remains `running` until the run finalizes, and incomplete
calls have their own status. A requested export
that fails to persist makes the process exit unsuccessfully.

`final_metrics` totals reported prompt tokens (uncached input plus cache reads
and cache creation), completion tokens and cache reads, without double-counting
usage updates within a call. Per-call `metrics.extra.native_usage`
preserves reasoning-output and cache-creation counts. These totals
cover reported usage; interrupted calls without final usage can have unknown
billable tokens. Dollar estimates and the run's normal failure/limit reporting
remain in the headless JSON result. ATIF capture is independent of the selected
stdout format.
