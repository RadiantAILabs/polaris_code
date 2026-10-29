# Permission modes

`polaris-code` resolves every tool call through the session's active
`PolicySnapshot`. The mode changes whether a call runs, asks, or is refused; it
does not change the file sandbox or the shell deny list.

| Mode | `edit_file`, `create_file` | shell tools | Use |
|---|---|---|---|
| `confirm` | asks | asks | Default interactive posture. |
| `auto-edit` | runs | asks | Interactive editing with visible diffs but no edit prompt. |
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

Interactive sessions accept `--permission-mode confirm|auto-edit|plan` and
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

### The ordering, and where it stops

The ceiling is defined over one ordering of the attended modes:

```
plan  <  confirm  <  auto-edit
```

That is the pointwise order on the per-tool gates (`Deny` < `Confirm` <
`Allow`): each rung up relaxes exactly one tool and tightens none. Asking a
human counts as a restriction, so a mode that asks sits strictly below the mode
that simply does it.

**Unattended is not a fourth rung.** It is a different shape. An unattended
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
| `confirm` | `confirm · edits and commands ask first` |
| `auto-edit` | `auto-edit · edits apply unasked, commands ask first` |
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

A ceiling is visible the same way — without being asked for, and before being
refused. An interactive session under a ceiling opens by saying which ceiling
is in force and which posture it starts in; the `/permission` popup then
advertises only the postures that can actually be entered and names the ceiling
that narrowed the list, and `/permission` with no argument repeats it.

The shell deny list remains authoritative in every mode, even when an
unattended allow pattern matches. The working-directory sandbox is also built
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
