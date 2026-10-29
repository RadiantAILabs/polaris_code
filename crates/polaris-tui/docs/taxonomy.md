---
title: polaris-tui Taxonomy
---

# polaris-tui Taxonomy

This document defines the conceptual layers and core abstractions in
`polaris-tui`. For the design philosophy these layers serve, see
[philosophy.md](./philosophy.md) (canonical copy:
https://github.com/RadiantAILabs/polaris/blob/main/docs/philosophy.md).

`polaris-tui` is a composable [ratatui](https://ratatui.rs) terminal UI built on
Polaris: a shared shell that plugins extend by contributing in-process panels.
Its layers are *this crate's own*, classified by the Polaris rule — **lower
layers are the fixed shell contract, upper layers are swappable panels that plug
into it.**

## Architecture

| Layer | Name | Owns |
|-------|------|------|
| **2** | Panels | the in-process panels plugins contribute (`conversation`, `sessions`, `logs`, `help`) |
| **1** | Shell & Panel Contract | the TUI runtime, the `TuiPanel` trait, the `TuiRegistry` / `TuiHandle` APIs, and the layout / nav / event / widget primitives |

The whole crate turns on one composability seam — the `TuiPanel` trait plus the
`TuiRegistry` in Layer 1 — that lets any plugin contribute a panel without the
shell knowing the panel's type, and without panels knowing about each other.

## Layer 1: Shell & Panel Contract

**Files:** `src/plugin.rs` (`TuiPlugin`, `TuiHandle`), `src/registry.rs`
(`TuiRegistry`), `src/panel.rs` (the `TuiPanel` trait), `src/runtime.rs`,
`src/app.rs`, `src/layout.rs`, `src/nav.rs`, `src/event.rs`, `src/widgets.rs`,
`src/composer/`, `src/config.rs`.

The fixed shell every panel plugs into:

- **The runtime.** `TuiPlugin` owns the terminal lifecycle, the draw/event
  loop, and focus/navigation across panels.
- **The panel contract.** The `TuiPanel` trait is the single interface a panel
  implements; `TuiRegistry` (an `API`) is where panels register, and
  `TuiHandle` (an `API`) is the handle other plugins use to drive the shell.
- **Shared primitives.** Layout, navigation, event routing, and the reusable
  widgets and editing buffers panels render with. `src/composer/` is one: a
  grapheme- and display-column-aware text buffer, bounded history navigation
  over a host-supplied store, and editor draft/argv preparation. These know
  nothing about coding policy. The runtime alone hands the terminal to an
  interactive child through `TuiHandle::run_in_terminal`; it never names a panel.

These define *how a panel attaches to the shell and how the shell drives it*.
Changing them is a breaking change to every panel. **Scope:** Layer 1 owns no
concrete panel content — it knows nothing about sessions, conversations, or
logs; it only defines the seam they meet on.

## Layer 2: Panels

**Files:** `src/panels/conversation.rs`, `src/panels/sessions.rs`,
`src/panels/logs.rs`, `src/panels/help.rs`, the private conversation
rendering helpers in `src/transcript/`, and `src/commands/` — the host ↔
conversation-panel contract for slash commands, `@` mention completion and
composer attachments (wire protocol, `CompletionSource`, `AttachmentSource`,
mention syntax, popup state machine).

The swappable, optional panels contributed through the Layer-1 registry:
`ConversationPanelPlugin`, `SessionsPanelPlugin`, `LogPanelPlugin`, and
`HelpPanelPlugin`. Each is a self-contained plugin that registers one
`TuiPanel`. A host application picks the panels it wants; downstream crates add
their own (e.g. ontrace's inspect/SPARQL panels) the same way.

**Scope.** A panel renders and handles input for its own content only. It must
reach the shell through `TuiRegistry` / `TuiHandle`, never reach into another
panel's internals, and must not assume which sibling panels are present. Host
policy stays with the host: the conversation panel ships no command set, no
file-system rules, no MIME table and no model knowledge — it presents what a
host advertises or sources through `src/commands/`, and dispatches what the
user picks back to the host. An attachment chip is the same bargain: the panel
draws it and stamps its handle, while whether a path may be attached, what it
contains, how large it may be and whether the model can read it are the host's
to answer.

## Boundary Guide

When adding functionality, place it by asking:

| Question | If yes | Examples |
|----------|--------|----------|
| Does it change how panels attach to or are driven by the shell? | **Layer 1** | A new method on the `TuiPanel` trait, a `TuiRegistry` API change, new shared navigation or layout behavior |
| Is it a self-contained piece of on-screen content? | **Layer 2** | A new panel plugin, a change to an existing panel's rendering or input handling |

Lower must not depend on upper: the Layer-1 shell names no concrete panel; a
Layer-2 panel depends on the shell contract, not the reverse, and not on its
siblings.
