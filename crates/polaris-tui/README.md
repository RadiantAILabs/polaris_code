# polaris-tui

A composable terminal UI for [Polaris](https://docs.rs/polaris-ai). Plugins
contribute in-process [ratatui](https://ratatui.rs) panels to a shared TUI
shell; the framework handles the terminal lifecycle, focus, layout, and a
small set of global key chords — your panel just renders a widget and handles
its own events.

`TuiRegistry` holds *in-process trait objects*: plugins add panels to it
during their build phase, and the shell drains it once the server is ready.

> Status: `0.1` — surface is small and may shift. Builds against `polaris-ai`
> 0.7, set in the workspace `Cargo.toml`.

---

## Workspace

| Crate                            | Path                          | Purpose                                                                 |
| -------------------------------- | ----------------------------- | ----------------------------------------------------------------------- |
| `polaris-tui`                    | `crates/polaris-tui`          | Framework: `TuiPlugin`, `TuiRegistry`, `TuiPanel` trait, runtime, built-in panels. |
| `polaris-tui-examples`           | `crates/polaris-tui-examples` | Runnable examples (`hello-tui`, `shell`).                               |

The repository root README lists the other workspace crates.

The framework crate ships four built-in panels behind the `panels` feature
(on by default): `conversation`, `logs`, `sessions`, `help`. Each has its
own contribution plugin so consumers pick the subset they want.

---

## Prerequisites

- A Rust toolchain (Cargo, edition 2024, rustc ≥ 1.97).
- Nothing else to install: Cargo fetches `polaris-ai` from crates.io at the
  version set in the workspace `Cargo.toml`.

---

## Quick start

### Run the multi-panel example

```bash
cargo run --bin shell
```

You'll get a tabbed shell with `Conversation`, `Uptime`, `Logs`, and `Help`
panels. Press `Tab` to cycle, `Ctrl-L` to toggle Single ↔ Tiled layout,
`Ctrl-C` to quit.

### Run the minimal example

```bash
cargo run --bin hello-tui
```

A single custom panel with a tick counter. Useful as a template — the file
is ~50 lines.

---

## Writing a panel

Implement the `TuiPanel` trait, then register the panel with the
`TuiRegistry` from a plugin's `build()` phase.

```rust
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use polaris_tui::ratatui::{Frame, layout::Rect, widgets::Paragraph};
use polaris_tui::{TuiConfig, TuiPanel, TuiPlugin, TuiRegistry};

struct HelloPanel;

impl TuiPanel for HelloPanel {
    fn id(&self) -> &str { "hello" }
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(Paragraph::new("hello, polaris"), area);
    }
}

struct HelloContribution;

impl Plugin for HelloContribution {
    const ID: &'static str = "myapp::hello_tui";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, server: &mut Server) {
        server
            .api::<TuiRegistry>()
            .expect("TuiPlugin provides TuiRegistry; the resolver orders it first")
            .add_panel(HelloPanel);
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TuiRegistry>(VersionReq::caret(TuiRegistry::CONTRACT_VERSION))
    }
}

#[tokio::main]
async fn main() {
    let mut server = Server::new();
    server
        .add_plugins(TuiPlugin::new(TuiConfig::default()))
        .add_plugins(HelloContribution);
    server.run().await;
    // TuiHandle::wait() resolves when the TUI exits.
    server.api::<polaris_tui::TuiHandle>().unwrap().wait().await;
}
```

The trait's full surface:

```rust
pub trait TuiPanel: Send + 'static {
    fn id(&self) -> &str;
    fn title(&self) -> &str { self.id() }
    fn init(&mut self) {}
    fn tick(&mut self) {}
    fn handle_event(&mut self, event: &TuiEvent) -> EventOutcome { EventOutcome::Continue }
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);
}
```

`handle_event` only sees `Key` / `Mouse` events when the panel is focused;
`Tick`, `Resize`, and focus transitions are broadcast to every panel
regardless of layout. A panel therefore never needs its own "am I focused?"
gate around input handling.

Panels are `Send`; the runtime owns them on the TUI task. State that needs
to be shared with the rest of the server should live behind an `Arc` /
`API` you clone in at construction time.

The built-in conversation panel also accepts an optional, host-owned status
report on the existing `io_type = "status"` channel. Use
`commands::report_status` with `StatusReport`; the panel can show opaque model,
workspace, session and approval labels, a projected `ContextWindow`, cost,
availability, and a count of host-owned tasks running outside the current turn
(`with_tasks_running`), without owning any of those policies. Unknown budgets and prices
remain visibly unknown, cumulative billed tokens stay separate, and an
unavailable report preserves drafts instead of accepting work no driver can
process.

---

## Panel building blocks

Beyond the `TuiPanel` trait, the framework ships a couple of pieces panels
otherwise reinvent:

- **`nav::KeyMap` / `nav::NavIntent`** — translate key presses into movement
  intents instead of hard-coding a key table. Stock styles
  (`KeyMap::arrows()`, `KeyMap::vim()`, `KeyMap::emacs()`, plus the
  typing-safe chord-only `KeyMap::vim_paging()`) compose with `.then()` —
  first match wins — and `KeyMap::conflicts()` reports overlapping keys
  before you layer two maps.
- **`widgets::ScrollableTranscript`** — scroll state for a wrapped,
  `Paragraph`-backed transcript. Clamps the offset against the *wrapped* row
  count and follows the tail as new content arrives, so paging can't scroll
  the content into the void.

```rust
use polaris_tui::{KeyMap, ScrollableTranscript};

// Arrow keys + vim's Ctrl-D/U paging chords — but not j/k, which would eat
// keystrokes meant for a text input.
let nav = KeyMap::arrows().then(KeyMap::vim_paging());

// in handle_event: if let Some(i) = nav.resolve(key) { transcript.apply(i); }
// in render:       transcript.render(frame, area, self.revision, || self.lines());
//                  (the closure runs only on a cache miss — when `revision` changes)
```

---

## Layout & key chords

The shell reserves a small global chord table so plugin panels don't need
to coordinate on it:

| Chord                | Action                                |
| -------------------- | ------------------------------------- |
| `Tab` / `Shift-Tab`  | Cycle panel focus.                    |
| `Alt-1` … `Alt-9`    | Jump directly to panel N.             |
| `Ctrl-L`             | Toggle Single ↔ Tiled layout.         |
| `Ctrl-S`             | Toggle selection mode (release/grab mouse capture). |
| `Ctrl-C` / `Ctrl-Q`  | Quit the TUI.                         |

`LayoutMode::Single` (default) renders one focused panel filling the
workspace, as a conversation-first layout; `LayoutMode::Tiled` tiles every panel with
the focused one highlighted. Toggle at any time with `Ctrl-L`; the initial
mode is set via `TuiConfig::with_layout`.

### Mouse & selection mode

When the host enables mouse capture (`TuiConfig::with_mouse(true)`), scrollable
panels (the conversation transcript, the log panel) respond to the **mouse
wheel** — three rows per notch — and the app owns the mouse, so the terminal's
own drag-select + copy are suppressed. Press **`Ctrl-S`** to drop into
*selection mode*: capture is released back to the terminal so its native
selection and clipboard work; press `Ctrl-S` again to re-grab the mouse for
in-app wheel scrolling. With mouse capture off (the default), the terminal
never loses the mouse, so native selection always works and `Ctrl-S` is a
no-op.

### Conversation input editing

The conversation prompt mirrors the iTerm / readline line-editing keys, so
the muscle memory from your terminal carries over. Each action lists both the
macOS arrow form and the emacs/readline chord — the input accepts either, so
it works whether or not your terminal maps Option to Meta.

| Keys                              | Action                          |
| --------------------------------- | ------------------------------- |
| `←` / `→`, `Ctrl-B` / `Ctrl-F`    | Move one character.             |
| `Opt+←` / `Opt+→`, `Alt-b` / `Alt-f` | Jump one word.               |
| `fn+←` / `fn+→`, `Home` / `End`, `Ctrl-A` / `Ctrl-E` | Jump to line start / end. |
| `Backspace` / `Delete` (or `Ctrl-D`) | Delete one character back / forward. |
| `Opt+Backspace` / `Ctrl-W`        | Delete the word before the cursor. |
| `Opt+Delete` / `Alt-d`            | Delete the word after the cursor.  |
| `Ctrl-U` / `Ctrl-K`               | Kill to line start / end.       |
| `↑` / `↓`                         | Move between draft rows; recall history at the first/last row when configured. |
| `Enter`                           | Submit the message.             |
| `Shift+Enter` / `Alt+Enter`       | Insert a newline (see [multi-line prompts](#multi-line-prompts)). |
| `\` then `Enter`                  | Insert a newline on any terminal. |
| `Ctrl-O`                          | Reveal hidden messages, expand diff previews, and show the agent-graph timeline. |
| `Ctrl-R`                          | Search prompt history; repeat to find an older match. |
| `Ctrl-G`                          | Open the draft in VISUAL/EDITOR; cancel when searching. |
| `Ctrl-V`                          | Attach the clipboard image, when the host supplies an `AttachmentSource` (see [attachments](#attachments)). |
| `Ctrl-X`                          | Remove the last attachment chip, and its `@` mention with it. |
| `/` at the start, `@` after a space | Open the command / file-mention popup (see [commands and mentions](#commands-and-file-mentions)). |
| `Tab`, `↑` / `↓`, `Esc` (popup open) | Accept the highlighted entry, move the highlight, close the popup. |
| `Esc`                             | Cancel history browsing/search; otherwise deny a pending approval. |
| `Ctrl-C`                          | Deny a pending approval, or clear the draft; press again to quit. |

### Multi-line prompts

The input grows as you type, up to ten rows, then scrolls its own contents. A
multi-line draft is submitted whole by `Enter`, and the frame's title shows how
many lines are queued up.

**`Shift+Enter` needs terminal support.** The legacy key encoding has no room
for a modifier on `Enter`, so most terminals cannot report `Shift+Enter` as
anything other than a plain `Enter`. The TUI negotiates the
[kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/)
at startup, which fixes this on the terminals that implement it — kitty,
Ghostty, WezTerm and foot among them. The negotiation is a probe, not a guess:
a terminal that does not answer simply keeps the legacy encoding.

On a terminal without it — macOS Terminal.app, older iTerm2 — **no**
modifier+`Enter` combination is distinguishable, `Alt+Enter` included. There,
end a line with a backslash and press `Enter`:

```text
› write a function that\
  takes a path and\
  returns its size
```

Backslashes are counted the way a shell counts them, so an odd run continues
the line and an even run is a literal escaped backslash. A pasted Windows path
ending in `\\` submits normally rather than becoming a continuation.

`Cmd+Enter` is deliberately not bound: Terminal.app and iTerm2 both claim
`Cmd+Return` for full-screen at the emulator level, so it never reaches the
application.

### Pasting

Bracketed paste is enabled at startup, so a multi-line paste arrives as one
payload and lands in the draft as one editable block. Its newlines do not
submit, and a pasted `/command` is inserted as text rather than dispatched.

Pastes are normalised on the way in: CRLF and lone CR become newlines, and C0
control characters are dropped (tabs and newlines excepted) so that pasted text
can never smuggle an escape sequence into the terminal.

### Prompt history and external editing

With a host history store configured, `↑`/`↓` recall accepted prompts at the
first/last display row. Returning past the newest prompt, or pressing `Esc`,
restores the draft and caret. `Ctrl-R` starts incremental substring search;
repeat it for an older match. `Enter` accepts the match into the editable draft
without submitting. `Esc`, `Ctrl-G`, or `Ctrl-C` cancels search.

The host opts in using `with_history(PromptHistoryConfig)` and supplies a
`PromptHistoryStore`, an explicit project/session namespace and a nonzero cap.
History is disabled by default. Storage runs off the render loop; accepted
prompts are cached immediately. Approval replies and failed sends are excluded.
Storage failures show a notice and retain the session cache. Failed writes are
not automatically replayed because a storage error may follow a successful
write. [Host integration and storage contract](docs/prompt-history-editor.md).

Outside search, `Ctrl-G` opens the draft in `$VISUAL`, falling back to `$EDITOR`
when VISUAL is unset or blank. If neither is set, the panel reports that an
editor needs configuring. Quote paths or arguments with spaces; command
substitution, variables, pipelines and globbing are not expanded. A host may
supply `EditorCommand::new(program).arg(argument)` through `with_editor` instead.
Use a foreground editor or a wait flag such as `code --wait`.

The runtime pauses input and drawing, releases terminal modes, waits for the
editor, and restores the screen and mouse-capture state. Success replaces the
draft without submitting it. A failed launch, nonzero exit or invalid output
leaves the draft and caret unchanged. Unicode and newlines survive; editor
output uses the same normalization as paste and is limited to 16 MiB. During
the handoff, the editor owns keyboard input. TUI shutdown terminates the active
editor process. Unix handoffs require POSIX `stty` to recover terminal settings
even when an editor exits without restoring them. These handoffs do not implement managed background shell tasks.

### Wide characters

The caret is positioned in display columns rather than character counts, so
CJK, emoji — including ZWJ sequences and flags — and combining marks all place
it under the glyph you see. Editing steps by grapheme cluster: one `Backspace`
over `é` removes the letter and its accent together, and one `←` crosses a
family emoji whatever it is built from.

### Tool approvals

When a tool's permission resolves to `ToolPermission::Confirm`, Polaris calls
`UserIO::confirm`, which sends an `io_type = "confirm"` request and reads the
`confirmed` metadata on the reply — an absent value counts as denial. The
conversation panel implements that contract:

- the request opens an **approval dialog**: its prompt — the command or edit
  being approved — enters the transcript under an `approve ❯` label, and a
  banner above the input names the answers that resolve it;
- `y` / `yes` / `approve` / `allow` / `ok` approves; `n` / `no` / `deny` /
  `reject` / `cancel` and `Esc` deny. Only that answer is stamped;
- anything else is an ordinary prompt — including anything typed *before* the
  request arrived. `TuiIOProvider` holds those back until the dialog is
  answered, so a queued message can neither approve the tool nor be swallowed
  as a failed answer; it arrives on the turn it was meant for.

A plain "yes" typed with no dialog open is just a message, and an answer that
cannot be delivered leaves the dialog open. Only a stamped, delivered approval
runs the tool. Which actions need approval is the tool's permission — set by
the consumer, not by this panel.

### Commands and file mentions

The panel ships **no commands of its own**. A host advertises the slash
commands it executes and supplies the candidates for `@` file mentions; the
panel presents both and dispatches what the user picks. A consumer that does
neither sees no popup, and `/` and `@` are ordinary text.

- **`/` at column 0** opens a popup of the host's commands with their
  descriptions, filtered as you type (prefix matches first, then substring).
  On `Enter`, a draft that starts with `/` is sent as a user message stamped
  `io_type = "command"` with `name` and `args` metadata — never as a prompt —
  and the host answers on the ordinary channel. An unknown name is dispatched
  too: the host, which alone knows its set, reports it. A pasted `/command`
  is inserted as text and dispatches only if you press `Enter`.
- **`@` at a word boundary** opens a popup of workspace paths from the host's
  `CompletionSource`. Queries run on a task and are cancelled by the next
  keystroke; a result that arrives after a newer query started is dropped.
  Accepted paths are inserted backslash-escaped (`@docs/read\ me.md`) so a
  path with spaces stays one token, and Unicode paths are matched and
  inserted as-is. Which paths exist, which are ignored, and how they rank is
  the host's policy; the panel only shows what it is given.
- **Keys claim only while the popup is open.** `Tab` accepts the highlighted
  entry, `Shift-Tab`/`↑`/`↓` move the highlight, `Esc` closes the popup until
  the draft changes. `Enter` fills in a partial match; once the query already
  spells the entry exactly, `Enter` submits. With no popup, every one of those
  keys keeps its usual meaning — `Tab` cycles panel focus, `Esc` denies an
  approval — and accepting or dismissing never touches text outside the token.
- The popup draws just above the meta line, borrowing rows from the
  transcript; it never covers the approval or delivery banners.

Hosts advertise with `commands::advertise_commands` (a `status` message
carrying a `commands` list) and read dispatches with
`commands::CommandInvocation::from_message`. To execute commands before the
agent sees them, wrap the panel's provider via
`ConversationPanelPlugin::with_io_decorator`; supply mention candidates via
`with_completion_source`.

A host may replace the visible conversation after a session switch by sending
`commands::session_replay_start`, canonical `IOMessage`s, then
`commands::session_replay_end`. The start event clears the previous transcript,
pending presentation state, timeline, and current-process token counters; the
end event explicitly says historical usage was not measured. These are
output-only presentation events: the panel never feeds replay back into the
agent input channel.

### Attachments

The panel ships **no file-system rules, no MIME table and no model knowledge** —
the same division it already draws for `@` mention candidates. A host that
implements `commands::AttachmentSource` and supplies it through
`ConversationPanelPlugin::with_attachment_source` turns a mentioned path, or
the clipboard, into a removable chip under the draft. A consumer that supplies
no source never grows a chip, `Ctrl-V` says so rather than doing nothing, and
nothing the panel sends carries attachment metadata.

- **A chip has one of two origins, and it remembers which.** A chip from an
  `@path` mention lives exactly as long as that mention does: editing the
  mention away drops the chip, and `Ctrl-X` deletes the mention text along with
  the chip, because the two are one fact. A clipboard chip has no mention
  behind it and stands on its own.
- **Each finished mention is offered to the host once.** A path the host
  answers `NotAttachable` — an ordinary source file — is not asked about again
  on the next keystroke and produces no notice, because mentioning source code
  is *supposed* to stay text. Only `AttachmentRejected::Failed` shows a
  message, and that string is the host's, shown verbatim, so it must say what
  to do about it.
- **`Enter` will not outrun a resolve.** Typing a path and submitting in one
  motion holds the draft and says why, rather than sending a prompt without the
  attachment the user had just asked for, or landing a refusal after the draft
  it applied to was gone.
- **The panel never sees a byte.** A submitted prompt carries the host's own
  opaque handles under the `attachments` metadata key, in chip order; the host
  minted them when it validated and read the file, and redeems them itself.
  Chips do not survive a session switch, since the handles name bytes the
  outgoing session's host was holding.

A host that supplies an `AttachmentSource` *and* a `TurnInputHandle` should
override `TurnInputHandle::submit_with_attachments`; the defaulted body
forwards to `submit` and would drop the handles. Because a terminal cannot read
the system clipboard itself — the emulator owns it and delivers only text
through bracketed paste — `AttachmentRequest::Clipboard` is a request a host may
be unable to serve, and answering with a reason plus the `@path` alternative is
the expected outcome on such a platform, not a defect.

### Input delivery

The channel into the agent is bounded, so a submission can be refused when the
agent is behind or has gone away. Nothing is lost or overstated when it is:

- the draft clears only once the channel **accepts** the message. A refusal
  leaves the text and cursor untouched, so `Enter` retries the same prompt —
  and cannot queue a second copy of it;
- a refused prompt is not echoed to the transcript, so it never looks delivered;
- a banner above the input distinguishes a full inbox (retryable) from a closed
  channel (not);
- an accepted prompt shows `· queued` until the agent actually takes it —
  "the channel took it" is not "the agent read it".

---

## Shell chords and panel precedence

The shell splits its own key bindings into two classes, so a panel can own a
chord that means something inside it without the shell losing control of the
app.

**Inviolable** — handled before any panel sees them, and no panel can claim
them. These are the guarantees that keep you from getting stuck:

| Keys | Action |
| ---- | ------ |
| `Ctrl-Q` | Quit. Always, from any panel state. |
| `Alt+1`…`Alt+9` | Focus panel N. Always. |
| `Ctrl-L` | Toggle single / tiled layout. |
| `Ctrl-S` | Toggle selection mode (only when the host enabled mouse capture). |

**Fallback** — offered to the focused panel first, and applied by the shell
only if the panel returns `EventOutcome::Continue`:

| Keys | Panel may claim it for | Shell does otherwise |
| ---- | ---------------------- | -------------------- |
| `Tab` / `Shift-Tab` | Accepting or cycling a completion | Cycle panel focus |
| `Ctrl-C` | Denying an approval, clearing a draft, or requesting interruption through a host-supplied handle | Quit |

A panel that claims nothing leaves the shell's own behaviour in place. A panel that
claims everything still cannot trap you: `Ctrl-Q` and `Alt+N` are inviolable,
and a second `Ctrl-C` within 1.5 s always falls through to the shell.

---

## Built-in panels

Each panel ships behind its own contribution plugin so you can pick
which subset to wire in.

| Plugin                       | What it contributes                                                                                               |
| ---------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `ConversationPanelPlugin`    | Chat transcript driven by a `TuiIOProvider`. Also registers a `UserIO` factory pointing at the same provider (wrappable with `with_io_decorator`), and answers its `UserIO::confirm` approval dialogs. Presents host-advertised `/` commands and host-sourced `@` file mentions (`with_completion_source`). A host can provide `with_interrupt_handle` so `Ctrl-C` requests interruption during a running turn; host-emitted `interrupting` and `cancelled` messages render the truthful lifecycle. `Ctrl-G` opens searchable transcript browsing, optionally paged back through a host-supplied canonical history (`with_history_source`). `Ctrl-O` reveals hidden messages, expands diff previews, and adds a per-turn agent-graph node timeline; pair `with_log_buffer` with `LogPanelPlugin::with_buffer` over one shared `LogBuffer` to show each node's captured `tracing` output. A host-supplied `with_attachment_source` turns mentioned paths and the clipboard into removable composer chips whose handles ride the submitted prompt. `with_background_handle` binds `Ctrl-T` to the host's request to background whatever the turn is waiting on — the panel forwards the press and learns nothing back, so the receipt and every refusal are host messages. |
| `LogPanelPlugin`             | Tail of `tracing` events fed by an installed `tracing-subscriber` layer.                                          |
| `SessionsPanelPlugin`        | Live list of active sessions read from `SessionsAPI`. Depends on `SessionsPlugin`.                                |
| `HelpPanelPlugin`            | Static keymap + plugin help. The simplest possible example.                                                       |

```rust
use polaris_tui::panels::{ConversationPanelPlugin, HelpPanelPlugin, LogPanelPlugin};

server
    .add_plugins(TuiPlugin::new(TuiConfig::default()))
    .add_plugins(LogPanelPlugin::default())
    .add_plugins(ConversationPanelPlugin::default())
    .add_plugins(HelpPanelPlugin);
```

To disable all built-ins and ship only the framework:

```toml
[dependencies]
polaris-tui = { git = "https://github.com/RadiantAILabs/polaris_code", default-features = false }
```

---

## Architecture

```
┌─ Server (polaris) ──────────────────────────────────────────────────┐
│                                                                     │
│   TuiPlugin                                                         │
│    ├── build()    → inserts TuiRegistry as an API                   │
│    ├── ready()    → drains panels, spawns runtime task              │
│    └── cleanup()  → signals shutdown, joins task                    │
│                                                                     │
│   Other plugins   → server.api::<TuiRegistry>().add_panel(…)        │
│                                                                     │
└─────────────────────────────────────────────────────────────────────┘
                                  │
                                  ▼
                        ┌─────────────────────┐
                        │  TUI runtime task   │   tokio::select!
                        │                     │  ─ crossterm events
                        │  AppShell           │  ─ tick interval
                        │   ├─ panels         │  ─ shutdown signal
                        │   ├─ focus state    │
                        │   └─ chrome render  │
                        └─────────────────────┘
```

- **Build phase** runs synchronously; contributors add panels to
  `TuiRegistry` with `add_panel`.
- **Ready phase** drains the registry and spawns the runtime task. The
  registry is empty after this point — late contributions are accepted but
  won't appear in the running shell.
- **Runtime task** owns the terminal (alternate screen + raw mode),
  pumps a `crossterm::EventStream`, ticks at `TuiConfig::tick_rate`, and
  renders the shell chrome plus panels.
- **Cleanup phase** sends a shutdown watch signal and joins the task with
  a 2-second timeout, then restores the terminal.

---

## Cargo features

| Feature   | Default | Enables                                                                 |
| --------- | ------- | ----------------------------------------------------------------------- |
| `panels`  | yes     | The four built-in panels and their contribution plugins.                |

---

## Compatibility

- `polaris-ai`: 0.7, as set in the workspace `Cargo.toml`.
- `ratatui`: re-exported as `polaris_tui::ratatui` so consumers don't have
  to pin a matching version themselves.
- `crossterm`: `0.29` with the `event-stream` feature, re-exported as
  `polaris_tui::crossterm` (the same crossterm `ratatui` drives), so panels
  matching on `TuiEvent::Key`/`Mouse` don't pin their own copy.

### Rich transcript

Messages tagged `io_type = "message"` or `"error"` render Markdown headings,
lists, inline code, tables and fenced code. Tool and approval previews retain
literal text, with diff styling only for identifiable unified diff headers. Errors
always have a red error label and block marker. Tool blocks show the supplied
`label` and `phase`; the panel does not interpret tool behavior. Plain messages
with no `io_type` retain their literal text, including Markdown punctuation.

Code highlighting covers Rust, Python, JavaScript, TypeScript and JSON using
basic lexical colors; unknown languages remain plain text. Indentation is
preserved, tabs expand at four-column stops, and terminal control characters
are removed before rendering. Incomplete fences render their available content.
Tables use cell separators and wrap in narrow terminals.

Unified diffs (bare previews or `diff`/`patch` fences) show file headers and
colored added/removed lines. Every preview body is bounded the same way, diff or
not: the default view shows 24 rows and explicitly counts the remainder, so a
tool that returns a whole file collapses instead of burying the conversation.
**Ctrl-O** expands it. Each message is limited to 64 KiB of source per render,
with a visible notice when truncated. The original body is retained within the
history policy; the preview is never written back into it.

`status` and `tokens` update only the meta line. Host-emitted `interrupting` and
`cancelled` messages add visible lifecycle rows; invoking an interrupt callback
alone adds no row. Unknown `io_type` messages are
retained but hidden by default, with a count and **Ctrl-O** hint at the start of
the meta line. Ctrl-O reveals them as literal text alongside the transcript and
node timeline. Ordinary transcript history retains 1,024 messages; unknown
messages are exempt from that eviction so hidden content remains recoverable
in-session. This means unknown-message retention can grow with the session.

### Live blocks

A host that streams its answer sends one block many times, and the panel folds
those updates so the user reads a message that grows rather than a column of
fragments. Folding is opt-in and adds no second protocol — it is the same
`io_type` metadata, keyed on an `id`:

| Key | On | Meaning |
|-----|-----|---------|
| `id` | `message`, `tool` | The block's stable identity. Without it, a message is standalone. |
| `seq` | `message` | Monotonic per `id`. An update that does not advance the block is dropped, so a resent or reordered delivery cannot print the same text twice. |
| `final` | `message` | `"true"` closes the block. A non-empty closing body replaces what the deltas accumulated, reconciling a lost one; an empty body keeps the accumulation. |
| `outcome` | `message` | Why it ended. `"error"` renders a "response ended early" marker under the partial text. |
| `parent` | `tool` | The `message` that requested it; the tool renders indented beneath it. |
| `phase` | `tool` | `pending`, `start`, `ok`, `denied`, `error` — rendered as a glyph. An unrecognised value still renders. |
| `label`, `elapsed_ms` | `tool` | The invocation's name, and its wall time once it reaches a terminal phase. |

A non-final `message` body is the **delta**, not the accumulated text, so a
producer forwards its provider's events without buffering the answer. Each tool
invocation stays on one row from `start` to its outcome, however many updates it
took to get there. The panel renders the lifecycle and knows nothing about what
any tool does.

### Transcript search, navigation, and export

`Ctrl-G` opens transcript browsing over the current view (the retained live
window, or a loaded history page). Browsing highlights one message at a time
and freezes your place against new output — the tail keeps growing
underneath; it just doesn't steal the scroll position or the selection.

| Key | Action |
| --- | ------ |
| `↑` / `↓` | Select the previous / next message. |
| `Shift-↑` / `Shift-↓` | Jump to the previous / next **user turn**, skipping everything else. |
| `/` | Edit a search query; `Enter`/`n` finds the next case-insensitive match, `Shift-Enter`/`N` the previous. Search wraps and is never delivered as a prompt. |
| `c` | Cycle the fenced code blocks inside the selected message. |
| `e` | Export the selection — the chosen code block if any, else the whole message — by typing a new file path and pressing `Enter`. |
| `a` | Export the entire visible transcript the same way. |
| `h` / `l` | Page to older / newer canonical history beyond the retained live window (see below). |
| `End` | Return straight to the live tail. |
| `Esc` | Close the browse overlay; while a search or export prompt is open, closes that instead. |

Export never overwrites an existing file, and status/token meta-lines and other
control metadata are excluded — only content a human actually wrote or read is
written out.

For terminals that support it, native mouse selection and copy remain
available the same way they do outside browse mode (see "Mouse & selection
mode" above): capture is released by default, so drag-select plus the
emulator's own copy shortcut work directly on rendered rows, and many
emulators keep their own scrollback reachable from inside the alternate
screen (a plain mouse-wheel scroll, or the emulator/multiplexer's dedicated
scrollback or copy-mode key). That route is terminal-dependent and only
reaches what your screen has recently shown; `Ctrl-G` browsing is the
app-native route, and the only one that can reach content evicted from the
retained window.

#### Retained history and evicted-message recovery

The live transcript keeps the most recent `1,024` messages; older ones are
dropped from memory to bound render cost over a long session. A host may wire
up a read-only history source (`ConversationPanel::with_history_source` /
`ConversationPanelPlugin::with_history_source`, contract in
`panels::conversation::TranscriptHistory`);
`h` inside browse mode then pages further back into the host's own canonical
record, `l` pages back toward the tail, and the banner reports the page range
against the host's total. A returned page **replaces** the previous one — it
is a read-only snapshot, never merged into live input, and can never execute a
tool or otherwise reach the agent. Without a history source, browsing,
search, and export still work over the retained live window; only paging past
it is unavailable, and the banner says so.

### Host-advertised command argument pickers

A host may attach finite `choices` to an advertised `CommandSpec`. After the
user types that command and a space, the conversation composer filters those
host-owned values in the existing completion popup; Tab or Enter replaces only
the argument draft. The TUI does not define models, permission modes, or other
settings itself. Commands submitted while an approval is open retain the draft
and use the existing queued-command path until that approval is answered.

### Queued follow-ups and active-turn steering

A host may install `ConversationPanelPlugin::with_turn_input_handle` to make the
run target explicit. Enter queues a follow-up and Ctrl-Enter offers steering to
the active run at the host's next safe boundary. Each accepted line says which
run will consume it. Alt-Up recalls the newest unconsumed item into the composer
for editing; Alt-Backspace removes it. Approval answers never enter this queue,
and a host that does not install the handle keeps the original `UserIO` delivery
path unchanged.

### Rewind reports

Hosts can emit a structured `commands::RewindReport`. The panel renders its
independent conversation, file, index, and persistence results without
interpreting checkpoint or filesystem policy, and keeps explicit untracked
effects visible. See [`docs/session-rewind.md`](docs/session-rewind.md).

---

## License

Apache-2.0.
