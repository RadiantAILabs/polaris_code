A catalog of every [`Plugin`](polaris::system::plugin::Plugin) shipped by
`polaris-tui`, grouped by the role it plays in the TUI shell.

Each entry links to the plugin's rustdoc page, which is the canonical source for
its contributed panels, provided APIs, dependencies, lifecycle, and a
registration example per the
[plugin documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris-tui/docs/reference/plugins.md#documentation-standard).
See the [integration guide](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris-tui/docs/reference/guide.md)
to go the other way — from a downstream goal to the plugins that satisfy it.

> **Drift guard.** This catalog is verified by an integration test
> (`tests/plugin_catalog.rs`) that scans `src/` for `impl Plugin for X` and
> asserts each name appears below. Adding a new plugin without listing it here
> will fail CI.

# Shell — the shared TUI runtime

The host every panel plugs into.

| Plugin | What it enables |
|--------|-----------------|
| [`TuiPlugin`](crate::TuiPlugin) | Owns the terminal: drains the [`TuiRegistry`](crate::TuiRegistry) in `ready()`, spawns a background task that pumps crossterm events and renders panels in single or tiled layout, and exposes [`TuiHandle`](crate::TuiHandle) for cooperative shutdown. Provides the `TuiRegistry` and `TuiHandle` APIs. |

# Panels — contributed content *(feature `panels`)*

Self-contained panel plugins that each register one `TuiPanel` through the
shell's registry. Add the ones you want; omit the rest.

| Plugin | What it enables |
|--------|-----------------|
| [`ConversationPanelPlugin`](crate::panels::ConversationPanelPlugin) | A scrollable conversation transcript panel, plus `TuiIOProvider` to bridge agent IO into it. Answers `UserIO::confirm` approval dialogs with the `confirmed` metadata that contract reads, and reports refused submissions instead of dropping them. Hosts may supply an interrupt callback; host-emitted `interrupting` and `cancelled` messages render its lifecycle without exposing a cancellation-token type. Renders tagged content as Markdown/code/diffs; errors stay visible, and unknown message kinds are retained with a Ctrl-O reveal and hidden count. Presents host-advertised `/` commands and host-sourced `@` file mentions in a completion popup and dispatches `/commands` to the host (`with_completion_source`, `with_io_decorator`; contract in [`commands`](crate::commands)). Folds `io_type = "message"` / `"tool"` updates sharing an `id` into one block each, so a streamed answer grows in place and a tool invocation stays one row from `start` to its outcome. `Ctrl-G` opens searchable transcript browsing — turn-jump navigation, code/text export to a file, and optional paged access to a host-supplied canonical history beyond the retained live window (`with_history_source`). Turns host-resolved paths and clipboard captures into removable composer chips whose opaque handles ride the submitted prompt (`with_attachment_source`; contract in [`commands`](crate::commands)). |
| [`SessionsPanelPlugin`](crate::panels::SessionsPanelPlugin) | A session list / switcher panel over `SessionsAPI`. |
| [`LogPanelPlugin`](crate::panels::LogPanelPlugin) | An in-shell `tracing` log panel backed by a `LogBuffer`. |
| [`HelpPanelPlugin`](crate::panels::HelpPanelPlugin) | A help / keybinding overlay listing the global chord table and registered panels. |

The conversation panel accepts a host-owned
[`PromptHistoryStore`](crate::PromptHistoryStore) through
[`ConversationPanelPlugin::with_history`](crate::panels::ConversationPanelPlugin::with_history),
with explicit project/session identity and a retention cap. The TUI caches
and searches prompts; the host supplies persistence. Its external editor can
be configured through [`EditorCommand`](crate::EditorCommand).

`ConversationPanelPlugin::with_turn_input_handle` optionally delegates queued
follow-ups and safe-boundary steering to a host-owned
[`TurnInputHandle`](crate::panels::conversation::TurnInputHandle). The panel
renders the returned run target and keeps approval replies on `UserIO`; it does
not decide where a safe boundary exists.

[`ConversationPanelPlugin::with_attachment_source`](crate::panels::ConversationPanelPlugin::with_attachment_source)
enables composer attachments through a host-owned
[`AttachmentSource`](crate::commands::AttachmentSource). The panel renders a
chip per resolved attachment, ties a chip from an `@` mention to that mention's
lifetime, refuses to submit over a resolve still in flight, and stamps the
host's opaque handles onto the prompt under
[`META_ATTACHMENTS`](crate::commands::META_ATTACHMENTS). Which paths are
attachable, what they contain, how large they may be, whether the active model
reads images, and whether this platform can produce a clipboard image are all
the host's to answer — the panel never opens a file and never carries a byte.
A host that also supplies a `TurnInputHandle` must override
[`TurnInputHandle::submit_with_attachments`](crate::panels::conversation::TurnInputHandle::submit_with_attachments),
whose defaulted body forwards to `submit` and drops the handles.

[`ConversationPanelPlugin::with_background_handle`](crate::panels::ConversationPanelPlugin::with_background_handle)
adds the second host callback, bound to `Ctrl-T`: the request to move whatever
the turn is waiting on into the background. It is
[`with_interrupt_handle`](crate::panels::ConversationPanelPlugin::with_interrupt_handle)'s
contract with a different verb — non-blocking, run on the UI task, and unbound
when a host installs none. The callback takes no argument and returns nothing,
so the panel cannot learn what is running or which work may be backgrounded;
the receipt, and every typed refusal, is a message the host sends. How many
such tasks are running is
[`StatusReport::with_tasks_running`](crate::commands::StatusReport::with_tasks_running),
one optional count on the meta row — a number, never a task list.

A [`session_replay_start`](crate::commands::session_replay_start) event is the
panel's session boundary: it drops everything it holds about the session it was
showing before the replayed transcript arrives. That includes the host's
[`StatusReport`](crate::commands::StatusReport) — model, workspace, session,
approval posture, projected context window, spend and availability — the last
error the host reported, the turn in flight, this process's token tally, and
any message still queued against the outgoing session's runs. Each goes back to
*not yet stated* rather than to a zero, so the meta row shows nothing until the
new session's host reports something. A host must therefore re-send its report
after a switch; it cannot clear a stale field by other means, because
[`StatusReport::merge`](crate::commands::StatusReport::merge) only ever sets the
fields an update asserts. The user's draft, the advertised command set, and a
delivery failure describing the channel rather than the session all survive the
boundary untouched.
