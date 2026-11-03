# Changelog

All notable changes to `polaris_code` are recorded here. Versions follow
[Semantic Versioning](https://semver.org/). Format: [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

Requires `polaris-tui` 0.1.2.

### Added
- Framed command approvals: `polaris-tui` exports `APPROVAL_SUBJECT` and `APPROVAL_CONTEXT`; a prompt carrying them shows the command once above the input, with the answers pinned under it and the full prompt behind `?`. `confirmed` and `approval_choice` still carry the answer, and `APPROVAL_CONTEXT` is a display hint only. With `APPROVAL_SUBJECT` the panel refuses a confirming answer until the whole command has been drawn, and accepts only a denial for a command over 64 KiB.
- Collapsed tool output: a finished `tool` block may carry a one-line `summary`, shown in place of the body until `Ctrl-O`. `run_command` blocks show shell text and keep the executor's JSON as `document` metadata; `stream-json` output is unchanged.
- `ConversationPanelPlugin::with_placeholder` and `DEFAULT_PLACEHOLDER` for the empty input's hint.
- `TurnInputHandle::is_pending` (defaults to `true`), so the panel can retire `queued for run N` badges on messages the host has taken.
- `CodeGraphPlugin::with_boot_index` and `tools::boot_index_wanted(headless, dir)`. An interactive `polaris` run outside a project skips the boot code index and says so; headless runs still index.

### Changed
- Approval labels state their scope: `allow this exact command in <dir> for this session`, `allow prefix "<prefix>" (whole words, every subcommand) in <dir> for this session`, `deny (or no: reason)`.
- A command too tall for the approval frame scrolls inside it (`↑`/`↓`, `PgUp`/`PgDn`), and a confirming answer is refused until its last row has been shown. A command over 64 KiB accepts only a denial.
- Default rendering every `polaris-tui` host gets: inline code is bold instead of yellow, a blank row separates turns, system messages and tool names/timings are dim, the empty input shows a placeholder, and the input title no longer repeats `Ctrl-N browse`.
- The meta row shows state, short model name (`Sonnet 5.5 · Bedrock`), context %, cost and running tasks; token tally, session, permissions, workspace and the full context window move behind `Ctrl-O`. Facts and footer hints are dropped whole, by priority, on narrow terminals instead of being clipped.
- Successful tool results longer than a line (other than diffs) collapse to a one-line summary; failures, denials and non-zero exits stay expanded.

### Fixed
- Approval text wraps by grapheme cluster, the way the terminal draws it, so an emoji or other multi-character grapheme can no longer push the end of a command out of view and still count as shown.
- Approval answer labels are bounded in size before they are laid out, so a very long label no longer slows every frame.
- The `p` (prefix) answer is left out when the prefix is very long, instead of showing an oversized label.
- On a terminal too small for the full answer labels, the approval footer collapses to the keys (`y s p n ?`) with the labels behind `?`, and a refusal reason shows in the frame's border title, so the answers and the rest of the command stay reachable.
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

[Unreleased]: https://github.com/RadiantAILabs/polaris_code/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/RadiantAILabs/polaris_code/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/RadiantAILabs/polaris_code/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/RadiantAILabs/polaris_code/releases/tag/v0.1.0
