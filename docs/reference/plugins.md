# Plugin Documentation Standard

`polaris_code` is a coding agent assembled from Polaris plugins: coding toolsets,
a graph-instrumentation bridge, and the reflective agent itself, all registered
on a `polaris::Server`. Every plugin's rustdoc is a contract: it tells a reader
(or a future contributor wiring a new capability) what the plugin puts on the
server, what it depends on, and how to register it. This page is the workspace's
normative standard; it adapts the canonical
[Polaris plugin standard](https://github.com/RadiantAILabs/polaris/blob/main/docs/reference/plugins.md#documentation-standard)
to this crate.

## Documentation Standard

A plugin's module- or struct-level rustdoc MUST open with a one- or two-sentence
**summary that states what the plugin does AND when to reach for it** (e.g.
"registers the sandboxed file toolset — add it so the agent can read and edit
files in the working dir"). Past the summary, the following sections apply.

### Required sections

Flag an ISSUE if any of these is missing.

| Section | Content |
|---------|---------|
| Summary + when-to-use | Opening paragraph: what it registers **and** when a consumer would pick it. |
| `# Resources Provided` | Table `Resource \| Scope \| Description`. A single `_none_` row with a one-line reason if it registers no globals/locals. |
| `# APIs Provided` | Table `API \| Description`. `_none_` row if it publishes none. |
| `# Dependencies` | `None.`, or the plugins/capabilities it needs (e.g. a toolset plugin needs `ToolsPlugin`; the agent needs `SessionsPlugin` + ontrace's `OxigraphPlugin`). |
| `# Example` | A `no_run` rustdoc block showing `add_plugins(...)` registration in dependency order. |

### Conditional sections

Flag an ISSUE only when the trigger applies and the section is missing.

| Section | Trigger |
|---------|---------|
| `# Tools Provided` | The plugin registers `#[tool]` / `#[toolset]` with a `ToolRegistry`. Table `Tool \| Permission \| Description`. |
| `# Middleware Registered` | The plugin installs graph middleware (e.g. the progress bridge). |
| `# Permissions` | A tool is gated (`ToolPermission::Confirm`/`Deny`) or constrained to the `working_dir` — state the trust boundary. |
| `# Lifecycle` | It has a non-trivial `ready()`/`cleanup()`, spawns a task, or is feature-gated. Bullet list of what happens and when. |

For the full rationale behind each section, see the canonical Polaris standard
linked above.

## Code graph language reach

`CodeGraphPlugin` indexes Rust (`.rs`), Python (`.py`, `.pyi`), JavaScript
(`.js`, `.jsx`, `.mjs`, `.cjs`), TypeScript (`.ts`, `.mts`, `.cts`, `.d.ts`),
and TSX (`.tsx`) into one Code `ABox`. The non-Rust parsers are embedded and
require no runtime server or download. Navigation relationships are syntactic;
the curated concept tools continue to describe only Rust/Polaris roles.
