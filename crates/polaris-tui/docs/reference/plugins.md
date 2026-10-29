# Plugin Documentation Standard

`polaris-tui` ships as a shared TUI shell (`TuiPlugin`) plus a set of panel
plugins that contribute in-process `TuiPanel`s to the shell's `TuiRegistry`. A
consumer adds the shell and whichever panels it wants to a `polaris::Server`.
Every plugin's rustdoc is a contract: it tells a downstream integrator what the
plugin contributes to the shell, what it depends on, and how to register it.
This page is the normative standard for plugin docs in this crate; it adapts
the canonical
[Polaris plugin standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/plugins.md#documentation-standard)
to this crate.

## Documentation Standard

A plugin's module- or struct-level rustdoc MUST open with a one- or two-sentence
**summary that states what the plugin does AND when to reach for it** (e.g.
"contributes a scrollable conversation panel to the TUI shell — use it when your
agent streams turns to a terminal UI"). Past the summary, the following sections
apply.

### Required sections

Every plugin's rustdoc includes these sections.

| Section | Content |
|---------|---------|
| Summary + when-to-use | Opening paragraph: what it contributes **and** when a consumer would pick it. |
| `# Resources Provided` | Table `Resource \| Scope \| Description`. A single `_none_` row with a one-line reason if it registers no globals/locals. |
| `# APIs Provided` | Table `API \| Description` (e.g. `TuiPlugin` provides `TuiRegistry` and `TuiHandle`). `_none_` row if it publishes none. |
| `# Dependencies` | `None.`, or the plugins/capabilities it needs. Panel plugins `requires::<TuiRegistry>()` — name that capability, not `TuiPlugin` directly, since the resolver binds by type. |
| `# Example` | A `no_run` rustdoc block showing `add_plugins(...)` registration (shell + this panel) plus a contributing `build()` that calls `add_panel`. |

### Conditional sections

A plugin's rustdoc includes each of these sections when its trigger applies.

| Section | Trigger |
|---------|---------|
| `# Panels Contributed` | The plugin calls `TuiRegistry::add_panel`. Name each panel's `id` and what it renders. |
| `# Keybindings` | The panel claims key chords beyond the shell's reserved global table. List `Chord \| Action`. |
| `# Lifecycle` | It spawns a background task (e.g. the shell's terminal runtime), has a non-trivial `ready()`/`cleanup()`, or is feature-gated. Bullet list of what happens and when. |

For the full rationale behind each section, see the canonical Polaris standard
linked above.
