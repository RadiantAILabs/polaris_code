# Shell approval contract

Install `tools::approval::ApprovalShellPlugin` alongside the framework's
`ShellPlugin` when the host grants individual shell calls. The approval plugin
depends on the shell plugin and replaces its standard `run_command` registration
with a receipt-aware tool before the registry freezes. The framework shell plugin
owns the executor and cleanup; background tools use that same executor.

Authorization belongs to host policy. After approval, the host issues an
`ApprovalReceipt` and injects it into `ToolContext`. Model JSON cannot mint a
receipt. An embedding host can use `ApprovalReceipt::issue` with its own
validator without installing the coding agent. The validator must compare the
current rule revision and session generation with those captured by the host.

A receipt binds the tool name, complete original arguments, canonical workspace
root and canonical command directory. The host supplies the directory displayed
before approval; changing a directory symlink invalidates authorization. Clones
share one consumption bit. The registered shell tool checks and consumes the
receipt immediately before confirmed execution. Executor deny rules and directory
containment still apply. Every dispatched call uses the registered tool's
`execute` implementation, so a replacement tool's refusal remains authoritative.

`SessionGrants` is a separate agent-policy resource for human-selected session
scopes. Exact and prefix grants never cross tool names or canonical directories;
a settings-generation change revokes them. Prefix patterns are prepared once
when remembered. `ShellApproval` resolves shell restrictions and dispatches via
the registry; it does not execute a process itself.

Approval prompts use `polaris_tui::ApprovalChoice` and the exported metadata keys
`APPROVAL_CHOICES` and `APPROVAL_CHOICE`, together with the framework's
`IO_TYPE_CONFIRM` and `CONFIRMED` protocol. `ApprovalRequest::ask` distinguishes
`ApprovalError::Denied` from `ApprovalError::Prompt`, preserving provider errors.
Interrupted prompts are resolved using their `APPROVAL_REQUEST_ID` before the
next input receive; an answer to a resolved prompt cannot approve a later call.

A command prompt also carries `APPROVAL_SUBJECT` (the command, verbatim) and
`APPROVAL_CONTEXT` (`run_command in <directory>`, home shown as `~`). The
conversation panel frames the command once above the input instead of printing
the whole prompt, with the answers pinned under it:

```
y allow once
s allow this exact command in /home/me/repo for this session
p allow prefix "cargo test" (whole words, every subcommand) in /home/me/repo for this session
n deny (or no: reason)
```

The labels state each answer's scope: the exact grant covers that command
string only, the prefix grant every command that starts with those whole words
(any subcommand or extra arguments), and both are bound to the named canonical
directory and end with the session. `p` is offered only when the command has a
simple prefix; `s` and `p` only where remembering is allowed. `n`, `no`, `deny`
and `Esc` deny; `no: reason` or `deny: reason` returns the reason to the model.

A command taller than the frame scrolls inside it (`↑` / `↓`, `PgUp` / `PgDn`
with an empty input), and the panel refuses a confirming answer until the
command's last row has been on screen. A command longer than 64 KiB cannot be
shown in full, so only a denial is accepted for it. `?` adds the full prompt —
tool, directory and JSON arguments — to the frame. On a terminal too small for
the labels above, the footer shows only the keys (`y s p n ?`) and `?` shows the
full labels; a refusal reason then appears in the frame's border title.

The reply's `confirmed` metadata and `approval_choice` still decide the answer,
and the grant is bound to the tool, arguments, canonical directory, generation
and rule revision as above; nothing in `APPROVAL_SUBJECT` or `APPROVAL_CONTEXT`
widens or narrows it. `APPROVAL_CONTEXT` is a display hint only.
`APPROVAL_SUBJECT` also makes the panel refuse a confirming answer until the
whole command has been drawn, and accept only a denial for a command over
64 KiB; that can only make the panel stricter. A front end that ignores both
keys shows the full prompt, which still names the command and directory.
