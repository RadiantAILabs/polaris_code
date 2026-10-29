# Prompt history and editor integration

`ConversationPanelPlugin::with_history` and `ConversationPanel::with_history`
accept `PromptHistoryConfig`. History is opt-in: the TUI owns a bounded cache
and its presentation; the host owns persistence and project policy.

```rust
use std::{num::NonZeroUsize, sync::Arc};
use polaris_tui::{PromptHistoryConfig, PromptHistoryNamespace, PromptHistoryStore};
use polaris_tui::panels::ConversationPanelPlugin;

fn conversation(store: Arc<dyn PromptHistoryStore>) -> ConversationPanelPlugin {
    ConversationPanelPlugin::default().with_history(PromptHistoryConfig::new(
        store,
        PromptHistoryNamespace {
            project: "/host/canonical/project".into(),
            session: "host-session-id".into(),
        },
        NonZeroUsize::new(500).unwrap(),
    ))
}
```

Implement `PromptHistoryStore::load` and `append` in the host. `load` returns
recent prompts oldest first, restricted to the supplied project. `append`
persists one accepted prompt with Unicode and line breaks intact. Both receive
the same explicit project/session namespace and cap. A host may aggregate
sessions within a project or isolate them further, but must not mix projects.
It chooses paths, retention, permissions, locking and durable writes. Store
methods execute serially per panel on blocking workers; multiple panels sharing
a store require host synchronization.

A submission enters history only after the provider channel accepts it.
Approval replies and backpressured/closed-channel submissions are excluded.
The panel caches accepted prompts immediately, including while loading the
stored snapshot. The cache and host retention are capped; Up/Down and search
become available after the initial load finishes or fails. Recreating a panel
with the same namespace reloads history through the host.

Storage errors do not undo a queued prompt or strand the composer. The banner
reports the failure; session-local recall remains available. Later writes can
succeed independently. Failed appends are not replayed automatically, since an
error may occur after data was written. The worker queue holds 64 appends; if
full or stopped, the panel reports that the prompt is cached only. Queued writes
drain after panel removal while the Tokio runtime remains alive. Hosts needing
a durable shutdown guarantee must flush their own store before runtime exit.

The consumer proof in `tests/prompt_history.rs` supplies a small file store,
recreates panels and host objects, and verifies project isolation and Unicode
round trips. It is a test fixture, not a prescribed production storage backend.

`Ctrl-R` searches the loaded cache. Search acceptance changes only the draft;
`Enter` must be pressed again to submit. Cancelling search restores the exact
previous draft/caret and does not answer any approval that arrived during the
search. `Esc` also cancels ordinary history browsing before acting on approvals.

`Ctrl-G` outside search edits externally. `VISUAL` takes precedence over
`EDITOR`; quoted whitespace and backslash escapes group literal arguments,
with no shell expansion. `with_editor(EditorCommand)` supplies a typed argv
alternative, including non-Unicode executable paths. The file path is a single
final argument. Editors must remain in the foreground or use their wait option.

The runtime gives each panel `TuiHandle` via the default
`TuiPanel::attach_runtime` hook before initialization. The conversation panel
starts its editor request on a background task. `run_in_terminal(Command)`
serializes child execution, pauses input and drawing, releases raw mode, mouse
capture, bracketed paste and keyboard enhancement, and leaves the alternate
screen. It resumes the previous modes and forces a repaint after exit. There
is no background event reader competing with the child for keyboard input.
On Unix, handoffs use POSIX `stty` to save and restore cooked terminal settings
across the child, including a failed editor that leaves raw mode enabled. If
`stty` is unavailable, launch fails recoverably with the original draft intact.
Shutdown kills and reaps the active direct child; this is an interactive UI
handoff, not a process-tree/background-task manager or permission-policy API.

Draft files use an exclusively created temporary directory (0700 on Unix) and
file (0600 on Unix). A successful foreground editor exit reopens the regular
UTF-8 file and replaces the draft, never submitting. Invalid UTF-8, nonregular
files, output over 16 MiB, spawn errors and nonzero exits preserve the original
draft/caret. CRLF and controls receive the same normalization as paste. The
library removes its draft file and empty directory after the round trip;
editor-created backup files follow that editor's own configuration.

The ignored `runtime::tests::unix_handoff_restores_termios_left_raw_by_a_failed_child`
test must be run in an isolated controlling PTY, never the developer's working
terminal. It runs a fake editor (`stty raw -echo; exit 9`) and compares complete
terminal settings before and after the handoff. Invoke the compiled library
test binary with `--exact runtime::tests::unix_handoff_restores_termios_left_raw_by_a_failed_child --ignored`
under a PTY harness. All other tests use a fake terminal or ratatui TestBackend.
