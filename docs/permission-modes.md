# Permission modes

`polaris-code` resolves every tool call through the session's active
`PolicySnapshot`. The mode changes whether a call runs, asks, or is refused; it
does not change the file sandbox or the shell deny list.

| Mode | `edit_file`, `create_file` | shell tools | Use |
|---|---|---|---|
| `confirm` | asks | asks | Default interactive posture. |
| `auto-edit` | runs | asks | Interactive editing with visible diffs but no edit prompt. |
| `auto` | runs | model reviews remaining confirmations | Attended automatic review with human fallback. |
| `plan` | refused | refused | Interactive read-only investigation. |
| unattended | fixed grant | fixed glob grant | Every `--print` invocation; it can never ask a human. |

`edit_file` and `create_file` are the two tools that write to the tree, and
they are one column on purpose: creating a file that does not exist yet is
gated on exactly the footing of editing one that does, in every mode. There is
no creation flag and no creation exemption. Both resolve their path through the
same sandbox rule, and `create_file` refuses a path that already exists rather
than overwriting it, so neither is a way around the other.

"Shell tools" is every tool that can put a command on this machine:
`run_command`, `run_in_background` and `stop_task`. They are named by one
constant (`SHELL_EXECUTION_TOOLS` in `src/agents/policy.rs`) and every mode
gives them one verdict, so moving a command into the background never turns a
prompt into a silent run. `task_output` only reads what a task already
produced, so it is allowed like any other read.

### Tools that need a person

A capability can declare that every attended call to one of its tools needs a
person, through the `ToolPolicyRegistry` in `src/tools/policy.rs`. `web_fetch`
and every MCP tool declare it. The agent policy then decides by mode, and the
tool's own `permission()` stays `Confirm`:

| Mode | `web_fetch` | MCP tool |
|---|---|---|
| `confirm`, `auto-edit`, `auto` | asks | asks |
| `plan` | asks | runs if its server is declared `readOnly: true`, otherwise refused |
| unattended (`--print`) | refused unless `--allow-tool web_fetch` grants it | refused unless `--allow-tool` names it exactly |

In every attended mode, including `auto`, a permission rule that allows the
call, a session grant or the automatic reviewer never answers for the person.
An explicit deny rule, a deny from the ceiling and a deny from the registry
still refuse the call. See [`web-access.md`](web-access.md) and
[`mcp.md`](mcp.md).

Interactive sessions accept `--permission-mode confirm|auto-edit|auto|plan` and
default to `confirm`. The prior session's mode is never persisted; an explicit
startup flag chooses the initial posture for that process, while in-process
`/new` and `/resume` transitions reset to `confirm`.

`--print` runs are unattended and fail closed. They deny edits and commands by
default. `--allow-edit` grants `edit_file` and `create_file` together — the
documented `-p "add a README"` example needs nothing more; each repeated
`--allow-command <GLOB>` adds one pattern to that run's immutable shell
allowlist. An unmatched command is refused rather than returned as a
confirmation request.

## The permission ceiling

The modes above answer "what is this session allowed to do now". A *ceiling*
answers a different question: "how permissive may this session ever become".
It exists for the case where the person who starts the process is not the
person driving it — a shared box, a demo, an automation wrapper, a colleague's
machine where the agent may read but must never run a command.

```bash
polaris-code --permission-ceiling plan          # read-only for the whole process
polaris-code --permission-ceiling confirm       # may never stop asking
```

Nothing reachable from inside the session — a command, a setting, a prompt, a
tool — can exceed the ceiling. `/permission` refuses a posture above it and
says which ceiling refused it; the session's posture is left exactly as it was.
Entering another session with `/new` or `/resume` resets to `confirm` as
before, lowered into the ceiling where the ceiling is stricter than that.

### Automatic review

Select `--permission-mode auto` or `/permission auto` to apply ordinary edits
and send remaining confirmations to the reviewer. Deterministic mode denials,
operator ceilings, permission rules, human session grants and shell hard denies
run first. Calls already allowed or denied make no reviewer request. Foreground
and background shell calls use the same review and exact-call receipt path;
`stop_task` is reviewed with its actual process-target arguments.

Some calls only a person may approve, and the reviewer never sees them:

- a call matched by an explicit `ask` rule;
- a call that only a project `allow` rule would allow, until
  `/permissions trust-project` enables that rule (an untrusted project allow
  rule asks);
- a call to a [tool that needs a person](#tools-that-need-a-person), such as
  `web_fetch` or an MCP tool.

In `auto`, these calls get the normal approval prompt, which says that automatic
review does not apply, and the permission trajectory records the person as the
decider. No reviewer request is made for them, and a reviewer answer for one
would be ignored at dispatch.

The explicit graph step `review_tools` runs after the main model and before
`execute_tools`. It produces typed per-call `ReviewResult` output. Dispatch
rechecks deterministic authorization and accepts a result only for a remaining
confirmation with matching arguments, call identity, canonical directory,
policy revision and settings generation. Its receipt is bound before the
reviewer wait and remains single-use. A review never grants a command prefix,
creates a remembered human scope or changes the sandbox.

The reviewer returns exactly `{"decision":"allow|ask|deny","reason":"..."}`:

| Result | Behavior |
|---|---|
| `allow` | Execute this exact invocation once through the existing receipt path. |
| `deny` | Return the reason as a tool error to the main agent. |
| `ask` | Show the normal human approval prompt with the review reason. |
| Timeout, cancellation, provider error, malformed or incomplete output, missing intent, unavailable model, refused budget | Require human approval; never infer an allow. Cancellation ends the pending approval without execution. |

`--review-model PROVIDER/MODEL` (or `POLARIS_CODE_REVIEW_MODEL`) selects a
registered reviewer model. By default it follows the session model serving the
current turn, including `/model` changes. The `AutoReviewPlugin` publishes a
`ReviewPolicy` with a 256-token output cap, 20-second timeout and three-denial
cutoff; embedding hosts can configure those limits. Each request reserves cost
through the session's `TokenMeter`; responses settle that reservation using
provider usage, including responses whose JSON is rejected. Interrupted or
failed requests settle an existing cost reservation at its estimate when usage
is unavailable.
There is no separate unmetered review budget. Reviews add cost and latency.

Review input contains the complete proposed tool arguments, canonical workspace
and working directory, deterministic policy facts and up to six recent real
user messages (4,000 characters each, with omissions marked). User text is
captured at the input boundary; restored history, compaction summaries, project
instructions, attachments, file contents and tool results are not copied into
review input. Agent-generated arguments occupy a separate assistant role and
cannot establish user authority. A reviewer has no tools. It is an advisory
filter, not an execution sandbox or a guarantee that a command is safe.

After three equivalent auto-denials in one turn, subsequent equivalent calls
ask the human without another reviewer request. Shell whitespace variations
share a denial counter; counters reset at the next real user turn. Review
outcomes appear in the transcript with the model, reason, elapsed time, token usage and estimated cost when available, and
are recorded with their source in the permission trajectory. Only actual user
approval can create a remembered session scope.

### The ordering, and where it stops

The original human-approval modes retain their restriction ordering:

```
plan  <  confirm  <  auto-edit
```

That is the pointwise order on the per-tool gates (`Deny` < `Confirm` <
`Allow`): each rung up relaxes exactly one tool and tightens none. Asking a
human counts as a restriction, so a mode that asks sits strictly below the mode
that simply does it.

`auto` is a separate capability, not a fourth rung. Its deterministic tool
gates resemble `auto-edit`, but its ability to approve commands with a model
requires explicit ceiling admission:

| Ceiling | `plan` | `confirm` | `auto-edit` | `auto` |
|---|---|---|---|---|
| `plan` | yes | no | no | no |
| `confirm` | yes | yes | no | no |
| `auto-edit` | yes | yes | yes | no |
| `auto` | yes | yes | yes | yes |
| none | yes | yes | yes | yes |

An `auto-edit` ceiling still requires a human for commands, so it refuses
`auto`. An `auto` ceiling permits the human-approval postures as well as model
review; startup and session reset still choose `confirm` unless explicitly
configured otherwise. A confirm or plan ceiling cannot be bypassed by a
reviewer allow, rule, setting or prompt.

**Unattended is outside both contracts.** It is a different shape. An unattended
policy turns every would-be confirmation into a refusal, so against `confirm`
it is *stricter* on everything it holds no grant for and *more permissive* on
everything it does: `--allow-command 'cargo *'` runs `cargo` with nobody asked,
which `auto-edit` would still confirm. Two unattended runs with different
grants are incomparable with each other for the same reason. Rather than guess
a rung, the ceiling refuses to order an unattended posture at all — so it can
never be named as a ceiling, and can never be admitted by one.

This is also why `--print` takes no `--permission-ceiling`, exactly as it takes
no `--permission-mode`. A `--print` run's posture is already immutable and
already fixed by the same operator on the same command line through
`--allow-edit` / `--allow-command`; a ceiling over it could only do nothing or
quietly redefine those grants, and the second would make the ceiling a second
way to spell a `--print` permission mode. Both flags are refused with a usage
error (exit code 2), and `--print` stays fail-closed unattended.

Where an explicit `--permission-mode` contradicts the ceiling the process
refuses to start rather than reinterpreting the operator's flags. Where only a
ceiling is given, the default `confirm` posture is lowered into it — so
`--permission-ceiling plan` starts the session in `plan`.

## Seeing the mode in force

A posture the user cannot read is a posture the user cannot rely on, so the
conversation's meta row states it on every status refresh:

| Mode | Meta row |
|---|---|
| `confirm` | `confirm · edits and commands ask first unless a rule allows them` |
| `auto-edit` | `auto-edit · edits apply unasked, commands ask first unless a rule allows them` |
| `auto` | `auto · edits apply unasked, a reviewer model decides commands no rule covers` |
| `plan` | `plan · edits and commands refused` |
| unattended | `unattended · nothing is ever asked, grants decide` |

The words are the host's — the TUI keeps no permission vocabulary and no mode
set — but the posture behind them is not a startup constant. It is read in
`prepare_context` from the same `PolicySnapshot::permission` the turn resolves
every tool call against, after the turn's settings snapshot has been applied.
So the row follows a mid-session `/permission` change on the turn that change
takes effect, and it reports the same label the trajectory reifies for each
tool decision. An unattended run is never described as confirming anything,
because an unattended policy turns a would-be confirmation into a refusal
rather than into a question.

`/permission` with no argument also replies with the active mode.

`/permission cycle` steps to the next posture the popup offers, in the popup's
order, and wraps around. It goes through the same ceiling check as a typed mode,
so it never reaches a posture above the ceiling. In the terminal UI, `Shift-Tab`
sends it; the reply names the mode it landed on, and the meta row follows on the
turn the change takes effect.

A ceiling is visible the same way — without being asked for, and before being
refused. An interactive session under a ceiling opens by saying which ceiling
is in force and which posture it starts in; the `/permission` popup then
advertises only the postures that can actually be entered and names the ceiling
that narrowed the list, and `/permission` with no argument repeats it.

The shell deny list remains authoritative in every mode, even when an
unattended allow pattern matches. An interactive session refuses a command on
the list before asking, so no approval or session grant is offered for a
command that could never run; the confirmed execution path applies the list
again as a second check. The working-directory sandbox is also built
identically in every mode. Shell patterns are literal-text globs, not a shell
parser: a broad pattern such as `cargo *` may also match command substitutions
or other shell syntax with side effects. `auto-edit` and unattended grants are
therefore intended only for environments where the operator already accepts
the process's full OS-account blast radius, such as a disposable container or
VM.

Secret redaction ([`secret-redaction.md`](secret-redaction.md)) is a separate
and much weaker thing. It rewrites recognisable credentials out of tool output
before that output reaches the model or a session file, in every mode. It is an
accident control, not a second permission boundary: a model that asks a tool to
transform the bytes first defeats it. The modes above are the boundary.

## Scoped command approvals

Command prompts offer host-defined choices:

- `y` / `yes`: allow this invocation once.
- `s` / `exact`: allow this literal command in the displayed canonical directory
  for this session.
- `p` / `prefix`: allow the displayed command prefix, at whole word boundaries,
  in that directory for this session. The prompt shows the exact prefix before
  it applies. Prefix choices are offered only for simple command text.
- `n` / `no` / Esc: deny. `deny: <reason>` also returns the reason to the model.

Grants are specific to a tool: granting `run_command` does not silently grant
`run_in_background`. `/new` and `/resume` clear them, including when reopening
an existing session. `/permissions` lists them and `/permissions clear-session`
revokes them. A ceiling that requires confirmation suppresses remembered
approval choices, and an explicit ask rule continues to ask on every call.

Each authorized interactive shell invocation receives a host-only, single-use
authorization bound to
its full arguments, canonical directory, permission revision and session
lifetime. A changed command, timeout, directory, policy revision or session
cannot reuse it. Confirmed shell execution still enforces deny patterns,
command length and the directory sandbox. This also covers commands with
opaque syntax such as `echo "$(pwd)"`: one human approval executes the approved
call once. The model cannot supply a `confirmed` argument to obtain authority.

Prefix and glob grants preserve the shell executor's command decomposition and
opaque-syntax checks. Each subcommand must match, so a `cargo test` prefix does
not grant `cargo test; curl example.org`. Substitutions and other opaque syntax
still ask. A human may deliberately remember that exact opaque command with
`s`; this does not grant variations of it. Broad interpreter and package-runner
prefixes can execute project code, including scripts that later change.

## Permission files

`PermissionRulesPlugin` reads these files once at process startup:

1. `$POLARIS_CONFIG_DIR/permissions.toml` when set; otherwise
   `$XDG_CONFIG_HOME/polaris/permissions.toml`, then
   `$HOME/.config/polaris/permissions.toml`.
2. `polaris.permissions.toml` at the repository root (the nearest ancestor with
   `.git`, or the working directory outside a repository).

Example:

```toml
[[rules]]
tool = "run_command"
decision = "allow"
command_prefix = "cargo test"

[[rules]]
tool = "run_command"
decision = "ask"
command_glob = "git push*"

[[rules]]
tool = "read_file"
decision = "deny"
path = ".env"

[[rules]]
tool = "create_file"
decision = "ask"
path = "private/*"
```

Each rule requires an exact tool name (or `*`) and `decision = "allow"`,
`"ask"` or `"deny"`. Optional command selectors are mutually exclusive:
`command` matches literal text, `command_prefix` matches simple literal words,
and `command_glob` supports `*` and `?`. File-tool `path` patterns match
canonical workspace-relative paths, including existing symlink aliases and the
parents of new files. They apply to tools that expose a `path` argument; they
are not a filesystem sandbox for shell scripts, search or indexing.

Across both files, **deny wins over ask, which wins over allow**, regardless of
file order. Mode denials, the permission ceiling, and shell hard denies remain
authoritative. Allow rules can authorize interactive calls within that ceiling;
they never widen unattended grants. Ask rules become refusals unattended.
Malformed or unreadable rule files refuse tool execution and `/permissions`
reports the errors with their source paths. Missing files mean no rules. A rule
file is read only if it is a regular file of at most 1 MiB; a larger file or a
pipe fails closed, so its rules are not parsed and the error is listed.

Project allow rules initially act as ask rules. Inspect `/permissions`, then
use `/permissions trust-project` to enable the already loaded project allow
rules for this process and to remember the decision for later sessions. The
saved decision is stored per project in the user configuration directory
(`project-trust.json`) and is bound to the exact bytes of
`polaris.permissions.toml`, so any change to the file asks again. If it cannot
be saved, the rules still apply for this process and the command says so.
Project deny/ask rules apply immediately. Files remain immutable snapshots:
changing a file requires restarting. Trust is never inferred from model text,
and an unattended run never trusts a project on its own. Saving a remembered
grant to a project file is not offered.

`/permissions` lists every rule, its source, project trust status and current
session grants. The deterministic authorization seam is
`PermissionPolicy::resolve_call`, followed by shell checks in
`ShellApproval::resolve`. A remaining `Confirm` goes to a person in `confirm`
and `auto-edit`. In `auto`, the `review_tools` graph step first sends it to the
reviewer, unless the decision is marked human-only (see
[Automatic review](#automatic-review)); only then is an `ApprovalRequest` built.
Without `AutoReviewPlugin`, every `Confirm` goes to a person, in `auto` too.

## Additional unattended tool grants

`--allow-tool <NAME|PATTERN>` can be repeated with `--print` (or an unattended
`--topology-task`) for additional tools. It accepts exact names and trailing
`*` prefixes. Generic wildcard grants never authorize MCP tools: those require
an exact `mcp__server__tool` name, and an MCP wildcard is a startup error.
File writers and shell tools retain their dedicated `--allow-edit` and
`--allow-command` flags. A tool grant cannot override a deny/ask rule or make an
opaque shell command run unattended.
