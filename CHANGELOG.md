# Changelog

All notable changes to `polaris_code` are recorded here. Versions follow
[Semantic Versioning](https://semver.org/). Format: [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

### Added

- Add an opt-in canonical conversation message ceiling, independent of context projection and the turn limit.

- Let plugins extend session configuration and append system-prompt sections through the coding-agent step registry.

- Let embedding hosts validate inputs before provider setup and opt out of automatic prompt routing.

- Expose the pure file edit planner so plugins can preview exactly what an edit would write.
- A plugin that depends on `PermissionRulesPlugin` can add a named permission rule written in Rust, with `PermissionRules::add_code_rule`, while the server is built. The rule is a function of the tool name, the call's arguments and the workspace root, and returns no opinion or a `RuleDecision` with a reason. It is weighed with the permission-file rules (deny over ask over allow), an allow never goes above the mode or the permission ceiling, and project trust does not apply to it. A denial reaches the model as a refusal that names the rule and is recorded as `denied` with the rule's reason. A rule that panics refuses the call. For `run_command` and `run_in_background` the rule is called with the whole command and then with each simple command. Rules cannot be added once the server is ready, and `/permissions` lists them by name. `PermissionRules` is now a mutable resource until the server is ready, and a global after that.
- Plugin-contributed coding-agent graph steps after tool rounds and before finish through `CodeAgentSteps` and `CodeAgent::with_steps`, with ordered finish holds that continue the existing bounded loop. `GraphViewPlugin::for_code_agent_steps` publishes the completed startup graph; an empty registry preserves the stock graph and agent identity.
- `--model-request-timeout-secs` / `POLARIS_CODE_MODEL_REQUEST_TIMEOUT_SECS` / `model_request_timeout_secs` configures Bedrock's native per-attempt request timeout for Converse and Responses. Accepts whole seconds from 1 to 86400; unset preserves existing defaults. Other providers refuse this setting because their builders expose no total request timeout. See the embedding guide for retry and streaming behavior.
- `polaris_code::startup`: the `polaris` binary's startup as a library. `Host::new().arg(..).plugins(..).run(args)` starts the same agent with the same flags, configuration files, environment variables, providers, front ends and exit codes, with plugins and command-line settings of the caller's own added, so another crate no longer copies `main.rs`. A host's plugins are added after all of `polaris`'s own and can add code permission rules (`PermissionRules::add_code_rule`) and coding-loop steps (`CodeAgentSteps`). `tabs::add_code_agent_shell_tabs` adds the shell's tabs with the Graph tab drawing the agent built from the step registry. `Host::run` returns the exit code instead of exiting the process.

### Changed
- `polaris` registers the coding agent built from the step registry (`CodeAgent::with_steps`) and its Graph tab uses `GraphViewPlugin::for_code_agent_steps`. With no steps added, the agent and the graph are the stock ones.
- A startup failure after the server has started (for example a model whose provider is not registered, or a session that cannot be created) now gives plugins their bounded cleanup before the process exits, as every other ending does. The message and exit code are unchanged.

### Fixed
- `edit_file`: an `old_string` that ends with a newline matches the end of a file that has no final newline, when the text before the newline occurs once in the file and that place is the end. The same number of newlines is left off the end of `new_string`, the file still ends without one, and the result says so. Other requests that do not match are still refused.
- `edit_file`: a refusal now says what to do next. An `old_string` that is not in the file shows the nearest text with line numbers and the first line where it differs; one that matches several places lists their lines. An empty `old_string` is refused for a file that has content (with `replace_all` it used to insert the new text between every character).
- A tool call whose arguments are not a JSON object (a reply cut off at the output-length limit while the call was being written) is answered with an explanation instead of `Parameters must be an object`.

## [0.4.2] - 2026-11-08

Requires `polaris-tui` 0.3.0 and `polaris-ai` 0.9.1.

### Fixed
- `Cargo.lock` now locks every Polaris crate under `polaris-ai` at 0.9.1. The 0.4.1 lockfile locked `polaris-ai` 0.9.1 but kept `polaris_internal`, `polaris_model_providers` and the other Polaris crates at 0.9.0, so `cargo install polaris_code --locked` built 0.4.1 without the OpenAI history fix below.

## [0.4.1] - 2026-11-08

Requires `polaris-tui` 0.3.0 and `polaris-ai` 0.9.1.

### Changed
- Uses `polaris-ai` 0.9.1. It fixes HTTP 400 errors from `--provider openai` and Bedrock `openai.*` models when an earlier model reply in the history had two or more text blocks. `polaris-tui`, `polaris_cosmos` and `polaris_ontrace` are unchanged at 0.3.0. Its lockfile still locked `polaris_model_providers` 0.9.0, so builds with `--locked` do not get this fix; use 0.4.2.
- `--trajectory-path` and `RunTrajectory` no longer write the file for each tool result or message. They are queued in memory and written by the next model call or by `finish`, so they never block an async worker. A hard kill can lose the tool results and messages recorded since the last model call.

## [0.4.0] - 2026-11-08

Requires `polaris-tui` 0.3.0 and `polaris-ai` 0.9.0.

### Added
- `--provider openai-chat` for OpenAI-compatible Chat Completions endpoints, including arbitrary vendor model ids.
- `--reasoning-effort` / `POLARIS_CODE_REASONING_EFFORT` / `reasoning_effort` pins effort on every model request, including auxiliary calls. The experimental Bedrock variable is retired with a startup migration error.
- `--context-window` / `POLARIS_CODE_CONTEXT_WINDOW` / `context_window` sizes projection and compaction after reserving output and prompt overhead.
- CLI-only `--dangerously-skip-permissions`, requiring `-p`, permits file access outside the working directory, edits and arbitrary shell commands without prompts in disposable containers. It removes the normal 100-iteration cap unless `--max-turns` is set; `--max-turns 0` explicitly disables the cap. Ordinary unattended runs retain explicit grants and their iteration default.
- CLI-only `--trajectory-path` exports reconstructable model requests, reasoning, tool calls, redacted results and native usage as durable ATIF JSON. Messages and configuration are stored once, with shared history ranges replacing repeated full requests. `ModelRunPlugin` and `RunTrajectory` expose the reusable provider policy and recording contracts.
- Tool results preserve both provider identifiers through execution, errors, cancellation and repaired history.
- Context budgets count replayed assistant reasoning according to the selected provider, including reasoning on tool-free replies. Compaction and history repair preserve complete reasoning blocks on retained messages.
- `polaris_cosmos::flow`: graph blocks for durable, budgeted, resumable agent workflows. The builders are `agent` (one role call: replay, admit, reserve, stage, run, settle, collect), `phase`, `memo`, `memo_current`, `repair_loop`, `lease_loop`, `rounds`, `worklist`, `counter`, `budget`, `events` and `bind`. They read thirteen global resources: `RunHandle`, `RoleBook`, `AgentRunner`, `StopFlag`, `SpendBook`, `StepJournal`, `RepairCounters`, `LeaseBook`, `FlowSettings`, `PhaseControl`, `Worklist`, `EventLog` and `CallDeadline`. Every durable file lives under one run root, and one process owns it.
- Resume: a run with `RunMode::Resume` restores only calls journaled as done. Skipped, failed and interrupted calls run again.
- `FlowPlugin`: registers the flow resources a host built (`with_resource`), charges calls left in flight by an earlier run at their ceiling when the server is ready, and stops phase heartbeat timers at cleanup. Hosts that wire resources by hand keep working.
- `flow::testing::ScriptedRunner` and `Scenario`: an offline, file-backed `TaskRunner` for tests.
- Host settings with neutral defaults, not constants: `LedgerSettings` (schema version, agent, provider and cost basis, set with `SpendBook::with_ledger`), the event log file name (`EventLog::with_file_name`, default `events.jsonl`), `FlowSettings::max_output_bytes` (default 64 MiB), and the enabled phase list of `PhaseControl::open` (a phase not in it does not run).
- Typed values in place of strings: `UsdAmount`, `RunMode`, `HaltReason`, `StepStatus`, `WorkItemStatus`, `ReserveError`, `SpendSummary`, and a `FlowError` enum that keeps the underlying I/O or JSON error. A persist fault while reserving is `ReserveError::Persist`, not a spent budget.
- `AgentConfig::system_prompt`, `command_globs` and `max_output_tokens`, with the builders `with_system_prompt`, `with_command_globs` and `with_max_output_tokens`, so each session can have its own prompt, shell command patterns and output limit. `system_prompt` replaces the built-in prompt and the project grounding block is still appended. `command_globs` limits the shell commands a session may run: every command in a chain or pipe must match a pattern, an empty list gives the session no shell tools (`run_command`, `run_in_background`, `task_output` and `stop_task` are not offered), and a non-empty list is shown to the model in its prompt and in each refusal. `max_output_tokens` bounds one reply and is the output allowance a spend cap reserves for each request; when it is unset the reservation assumes the Anthropic fallback of 4096 tokens, whatever the provider. Of the three, `polaris` sets only `max_output_tokens`, to the limit its model provider is registered with.
- `tools::CallRoot`, a tool-context value holding the folder one call works in. The agent's tool lanes insert it from `AgentConfig::working_dir` on every call, so sessions on one server work in their own folders. The file tools, `run_command` and `run_in_background` follow it: the file tools refuse a path that leaves it and `run_command` refuses a `working_dir` that leaves it. The folder is where a command starts, not an operating-system sandbox. A toolset used without a `CallRoot` keeps the directory it was built with. The code-graph, memory and MCP tools stay server-wide.
- Background tasks carry the `CallRoot` they started under. `task_output` and `stop_task` called with a `CallRoot` see only tasks with the same one; a host call without a `CallRoot` still sees every task.
- `tools::command_check::checked_commands`, which splits a shell command into the simple commands it would run and returns them only when every one matches a pattern. It returns `None` when the command is refused, including any command it cannot fully parse. The module is public but hidden from the rendered documentation.
- `TokenMeter::task_cost_spent_usd`, the dollars a task spend limit counts as spent: billed cost of finished requests plus the estimate of requests cut off in flight. It is `None` when no limit is set.
- Release archive `polaris-code-<version>-x86_64-unknown-linux-musl.tar.gz` (with its `.sha256` file): a static Linux x86-64 executable for glibc and musl images, built by `scripts/build-portable-binary.sh`. CI checks that it has no dynamic library dependencies and no dynamic loader. Homebrew still uses the GNU archive.
- Harbor adapter (`evals/packs/terminal_bench_harbor`): the agent keyword `version=` installs a pinned release of that musl archive (Linux x86-64 task images only, checksum checked, optional `sha256=` digest pin) instead of uploading a local `binary=`. New keywords `provider`, `base_url`, `reasoning_effort`, `context_window`, `max_output_tokens` and `unattended`. Harbor's token and cost fields come from the native ATIF trajectory. `unattended=true` together with `allow_command` is refused when the agent is created. CI runs the adapter tests against Harbor 0.24.0.

### Changed
- Uses `polaris-ai` 0.9.0. `polaris-tui`, `polaris_cosmos` and `polaris_ontrace` are released as 0.3.0 against it.
- `polaris_cosmos` now depends on `async-trait` 0.1, `toml` 0.8 and `tokio` 1 (features `macros`, `rt`, `sync`, `time`). The single `polaris-ai` is unchanged.
- A spend cap now applies per call. When a session's `TokenMeter` carries a cap (`TokenMeter::set_task_cost_cap`), the agent reserves the most each model request could cost before sending it, replaces the reservation with the billed usage when the reply ends, and ends the call with a failure beginning `cost-cap:`, without contacting the model, when the reservation does not fit in what is left. The reservation is given back only when the provider refused the request with an authentication, rate-limit or invalid-request error; a request cut off in flight, one that fails any other way, and one whose reply does not report both token counts are charged at their estimate. The operator's `--max-cost-usd` and `--max-tokens` still set no such cap.
- `AgentConfig` has three new public fields and is now `#[non_exhaustive]`. Build it with `AgentConfig::new` and the `with_*` builders; a struct literal outside the crate no longer compiles. Its public fields can still be read and assigned.
- The file tools and the shell now work in the session's `AgentConfig::working_dir` rather than the directory the toolset was built with, which differs only for a host that gave the two different folders.

## [0.3.0] - 2026-11-03

Requires `polaris-tui` 0.2.0 and `polaris-ai` 0.8.0.

### Added
- Approval picker: an approval request opens a picker in place of the input, with the command once and a numbered list of answers. One key press answers (a digit, the answer's own key, or `Enter`); `Esc` and `Ctrl-C` deny; `Tab` adds a one-line note; `?` shows the full prompt. Text typed while the picker is open is dropped, so it can neither answer the approval nor start a turn.
- `polaris-tui` exports `APPROVAL_SUBJECT`, `APPROVAL_CONTEXT` and `APPROVAL_NOTE`. With `APPROVAL_SUBJECT` the panel refuses a confirming answer until the whole command has been drawn, and accepts only a denial for a command over 64 KiB. `APPROVAL_CONTEXT` is a display hint only; `confirmed` and `approval_choice` still carry the answer.
- Approval notes: a note on an allow is sent as `approval_note`, recorded with the human decision, and added to that call's tool result as `User note on approval: ...` (control characters removed, cut to 1 KiB, secrets redacted). A note on a denial is the reason (`deny: <note>`). A note never changes the grant, and session grants do not carry it to later calls.
- `HumanApproval`, an optional global resource, and `InteractiveApprovalPlugin`, which provides it. Interactive runs install the plugin; headless runs do not. With no approver, a call that needs a human is denied (`Permission denied: no approver is registered`), never allowed.
- Collapsed tool output: a finished `tool` block may carry a one-line `summary`, shown in place of the body until `Ctrl-O`. `run_command` blocks show shell text and keep the executor's JSON as `document` metadata; `stream-json` output is unchanged.
- `ConversationPanelPlugin::with_placeholder` and `DEFAULT_PLACEHOLDER` for the empty input's hint.
- `Debug` for `ConversationPanelPlugin`.
- `TurnInputHandle::is_pending` (defaults to `true`), so the panel can retire `queued for run N` badges on messages the host has taken.
- `CodeGraphPlugin::with_boot_index`, `tools::boot_index_wanted(headless, dir)` and `tools::looks_like_project`. An interactive `polaris` run outside a project skips the boot code index and says so; headless runs still index.

### Changed
- `CodeAgentPlugin` no longer asks a person on its own. Hosts must add `InteractiveApprovalPlugin` or supply their own `HumanApproval`; otherwise every call that needs a person is denied.
- `polaris-tui` replaces typed `y`, `n` and `no: reason` approval answers with the picker. Press `Tab` to enter a reason or note.
- `polaris-ai` moves from 0.7.1 to 0.8.0. Hosts must upgrade their framework dependency too; all public types exposing framework types now use 0.8.0.
- `ApprovalAnswer` is `#[non_exhaustive]`. Build it with `From<ApprovalScope>` or its constructor instead of a struct literal.
- `polaris_cosmos` 0.2.0 upgrades to `polaris-ai` 0.8.0, with no source changes.
- `polaris_ontrace` 0.2.0 upgrades to `polaris-ai` 0.8.0, with no source changes.
- Tool calls run through a per-call graph loop (`authorize_call`, `ask_human`, `execute_call`, `record_denial`, `advance_call`) with its own iteration bound, instead of one batch step. Read-only batches still run concurrently. Calls left over at the bound get error results.
- Approval labels state their scope: `allow this exact command in <dir> for this session`, `allow prefix "<prefix>" (whole words, every subcommand) in <dir> for this session`, `deny`.
- A command too tall for the approval picker scrolls inside it (`PgUp`/`PgDn`, `Shift-↑`/`Shift-↓`), and a confirming answer is refused until its last row has been shown. A command over 64 KiB accepts only a denial.
- Default rendering every `polaris-tui` host gets: inline code is bold instead of yellow, a blank row separates turns, system messages and tool names/timings are dim, the empty input shows a placeholder, and the input title no longer repeats `Ctrl-N browse`.
- The meta row shows state, short model name (`Sonnet 5.5 · Bedrock`), context %, cost and running tasks; token tally, session, permissions, workspace and the full context window move behind `Ctrl-O`. Facts and footer hints are dropped whole, by priority, on narrow terminals instead of being clipped.
- Successful tool results longer than a line (other than diffs) collapse to a one-line summary; failures, denials and non-zero exits stay expanded.

### Fixed
- Approval text wraps by grapheme cluster, the way the terminal draws it, so an emoji or other multi-character grapheme can no longer push the end of a command out of view and still count as shown.
- Approval answer labels are bounded in size before they are laid out, so a very long label no longer slows every frame.
- The `p` (prefix) answer is left out when the prefix is very long, instead of showing an oversized label.
- Approval labels, the context and the prompt's first line are cut to 2 KiB on arrival (a cut one ends in `…`).
- On a very small terminal the picker shrinks to the highlighted answer and its hint always keeps `Esc deny`, so the answers and the rest of the command stay reachable. With no room for any of the command, only a denial is accepted until the terminal is enlarged.
- A confirming answer is refused while part of the command is too wide to draw (shown as `�`), and a burst of scroll keys between frames can no longer skip rows; a new width starts the read again from the top.
- Zero-width spaces, line and paragraph separators and byte-order marks are removed from transcript and approval text, like other invisible controls.
- `TurnSteering::is_pending` returns `true` when its queue lock is poisoned, matching the trait's documented default, so a queued message keeps its `queued for run N` badge.
- `home_dir` falls back to `USERPROFILE` when `HOME` is empty, as its documentation says.

## [0.2.0] - 2026-11-02

Requires `polaris-tui` 0.1.1.

### Added
- Graph tab: shows the agent graph live, with the running node marked (`GraphPanelPlugin`, `GraphViewPlugin`, `GraphFeed`).
- Smooth scrolling in the transcript, with redraws grouped at 60 fps.
- Built-in Bedrock model list (`config::BUILTIN_BEDROCK_MODELS`).
- `polaris login` / `logout` / `login status`, `--list-models` and model aliases.
- Layered `config.toml` / `polaris.toml` with profiles; `--max-turns`, `--max-output-tokens`, `/status`.
- Permission rules, `/permissions`, `--allow-tool`, and `--permission-mode auto` with a reviewer model.
- Context compaction (`/compact`), MCP client (`/mcp`), and a `web_fetch` tool.
- TUI keys: Esc interrupts, Ctrl-G opens `$EDITOR`, Ctrl-N browses the transcript, Shift-Tab steps the permission mode; `--notify`.

### Changed
- The Logs tab is folded into the Ctrl-O node timeline; default tabs are Conversation, Graph, Help.
- Ctrl-C first returns to the live bottom when the transcript is scrolled up.
- Default model is `claude-sonnet-5-5`, on Bedrock too.
- Project `.mcp.json` servers and project permission files apply only after the user trusts them.
- Model-call retries are graph steps, with backoff that honours `Retry-After`.
- Public API changes in `CommandOutcome`, `RetryPolicy`, `PermissionPolicy` and approvals (see PR #1).

### Fixed
- Homebrew Linux archive runtime linkage.

## [0.1.1] - 2026-10-29

### Added
- The `polaris` command and Homebrew installation.

## [0.1.0] - 2026-10-29

### Added
- First public release of `polaris_code` and its supporting crates.

[Unreleased]: https://github.com/RadiantAILabs/polaris_code/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/RadiantAILabs/polaris_code/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/RadiantAILabs/polaris_code/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/RadiantAILabs/polaris_code/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/RadiantAILabs/polaris_code/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/RadiantAILabs/polaris_code/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/RadiantAILabs/polaris_code/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/RadiantAILabs/polaris_code/releases/tag/v0.1.0
