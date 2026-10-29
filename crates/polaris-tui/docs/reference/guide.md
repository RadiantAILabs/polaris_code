# Integration Guide

This guide goes from a downstream goal to the `polaris-tui` plugins that satisfy
it. It is the reverse index of the [plugin catalog](../../src/docs/plugins.md):
the catalog answers "what does this plugin do?", this guide answers "I want to
do X — what do I register?".

## Common Integration Patterns

| Goal | Register | Notes |
|------|----------|-------|
| **Stand up a terminal UI shell** | `TuiPlugin` | Owns the terminal, the event loop, focus/layout. Provides `TuiRegistry` (panels contribute) and `TuiHandle` (drive shutdown). Add it before any panel. |
| **Show the live agent conversation** | `TuiPlugin` + `ConversationPanelPlugin` *(feature `panels`)* | Contributes a scrollable transcript panel; pairs with `TuiIOProvider` to bridge agent IO into the panel. |
| **Approve a tool before it runs** | `ConversationPanelPlugin` *(feature `panels`)* | Answers `UserIO::confirm`: the request opens an approval dialog showing the command or edit, and `y`/`n` (or `Esc` to deny) is delivered with the `confirmed` metadata `confirm` reads. Ordinary prompts typed around it keep their own turn. |
| **Interrupt a running turn** | `ConversationPanelPlugin::with_interrupt_handle` *(feature `panels`)* | Supply a non-blocking host callback. After approval and draft precedence, `Ctrl-C` invokes it once while a host status is active; a second press within 1.5 seconds falls through so the shell quits. Emit `io_type = "interrupting"` when the signal is raised and `io_type = "cancelled"` when the turn stops; only those messages render lifecycle rows. |
| **Background what a turn is waiting on** | `ConversationPanelPlugin::with_background_handle` *(feature `panels`)* | Supply a second non-blocking host callback. `Ctrl-T` invokes it — the Unix `Ctrl-Z` is deliberately not used, because a host that has not put the terminal in raw mode would suspend the process instead. The panel learns nothing back: it does not know what is running, which work may be backgrounded, or whether the host agreed, so every consequence is an ordinary host message. Report the outcome as a `tool` update on the same block (`phase = "background"` needs no new panel vocabulary — unrecognised phases render) and a typed `io_type = "error"` for a refusal. Without a handle the chord is unbound. |
| **Say that work is running off-screen** | `commands::StatusReport::with_tasks_running` *(feature `panels`)* | One optional count on the report the meta row already renders, shown as `2 running` beside model / context / cost. It is a number, not a task list: the panel holds no registry, no identity type and no lifecycle. Report `0` to retract it — merging only ever sets the fields an update asserts, so silence cannot clear a stale count. |
| **Attach workspace files or clipboard images** | `ConversationPanelPlugin::with_attachment_source` *(feature `panels`)* | Supply a `commands::AttachmentSource`. A resolved `@path` mention or `Ctrl-V` clipboard capture becomes a removable chip (`Ctrl-X`), and the submitted prompt carries the host's opaque handles under `META_ATTACHMENTS`. Containment, content type, size and model capability are all the host's to decide; answer `NotAttachable` for "this is just text" and `Failed(reason)` for a refusal worth showing. Also override `TurnInputHandle::submit_with_attachments` if you supply a queue. |
| **Let a panel own a shell chord** | `TuiPanel` + `EventOutcome` | Return `EventOutcome::Consumed` from `handle_event` to claim a fallback chord (`Tab`, `Shift-Tab`, `Ctrl-C`); return `Continue` to leave the shell's behaviour untouched. `Ctrl-Q`, `Alt+N`, `Ctrl-L` and `Ctrl-S` are inviolable and never reach a panel. |
| **Survive a busy or departed agent** | `ConversationPanelPlugin` *(feature `panels`)* | A bounded channel that refuses a prompt never clears the draft or echoes it as delivered; a banner distinguishes retryable backlog from a closed channel, and accepted prompts show `· queued` until the agent takes them. |
| **Browse / switch sessions** | `TuiPlugin` + `SessionsPanelPlugin` *(feature `panels`)* | A session list/switcher panel over `SessionsAPI`. |
| **Tail logs in-shell** | `TuiPlugin` + `LogPanelPlugin` *(feature `panels`)* | A `tracing` log panel backed by a `LogBuffer`. |
| **Offer a keybinding/help overlay** | `TuiPlugin` + `HelpPanelPlugin` *(feature `panels`)* | Renders the global chord table and registered panels. |
| **Contribute your own panel** | a plugin that `requires::<TuiRegistry>()` and calls `add_panel(MyPanel)` in `build()` | Implement `TuiPanel`; the shell drains the registry in `ready()`. See the crate-level Quick Start. |
| **Cooperative shutdown (e.g. from an HTTP route)** | hold `TuiHandle` and call `shutdown()` | Idempotent; the shell joins its runtime task in `cleanup()`. |

The panel plugins live behind the `panels` feature; the bare shell + the
`TuiRegistry` / `TuiHandle` / `TuiPanel` contract are always available.
