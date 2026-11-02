# Changelog

All notable changes to `polaris_code` are recorded here. Versions follow
[Semantic Versioning](https://semver.org/). Format: [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased]

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
