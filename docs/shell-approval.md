# Shell approval contract

Install `tools::approval::ApprovalShellPlugin` alongside the framework's
`ShellPlugin` when the host grants individual shell calls. The approval plugin
depends on the shell plugin and replaces its standard `run_command` registration
with a receipt-aware tool before the registry freezes. The framework shell plugin
owns the executor and cleanup; background tools use that same executor.

Authorization belongs to host policy. Before asking, the host binds an
`ApprovalReceipt`; only an allow decision injects it into `ToolContext`. Model JSON cannot mint a
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
with an empty input), and the panel refuses a confirming answer until every row
of the command has been on screen, and while any part of it is too wide to draw. A command longer than 64 KiB cannot be
shown in full, so only a denial is accepted for it. `?` adds the full prompt —
tool, directory and JSON arguments — to the frame. On a terminal too small for
the labels above, the keys move into the frame's top border (`y s p n · ? details`),
`?` shows the full labels above the command, and a refusal reason appears in the
bottom border. With no room for any of the command, only a denial is accepted
until the terminal is enlarged.

The reply's `confirmed` metadata and `approval_choice` still decide the answer,
and the grant is bound to the tool, arguments, canonical directory, generation
and rule revision as above; nothing in `APPROVAL_SUBJECT` or `APPROVAL_CONTEXT`
widens or narrows it. `APPROVAL_CONTEXT` is a display hint only.
`APPROVAL_SUBJECT` also makes the panel refuse a confirming answer until the
whole command has been drawn, and accept only a denial for a command over
64 KiB; that can only make the panel stricter. A front end that ignores both
keys shows the full prompt, which still names the command and directory.

The agent graph authorizes one call at a time. `prepare_calls` creates a local
queue; `authorize_call` re-reads session grants and checks a review against the
original call index and current policy. An allowed call reaches `execute_call`,
a refusal reaches `record_denial`, and an unresolved confirmation reaches
`ask_human`. The tool loop has its own iteration bound, independent of
`--max-turns`. If that bound is reached, every remaining call receives an
explicit error result. Tool failures remain results, so later calls can run.
Entirely allowed read-only batches retain their concurrent execution path.

Install `agents::InteractiveApprovalPlugin` to publish the optional global
`HumanApproval`. `ask_human` reads it as `Option<Res<HumanApproval>>`; without
an approver, it returns "Permission denied: no approver is registered" and the
batch continues. Permission modes that allow calls without asking still work.
An embedding can replace the transport through `HumanApproval::new` and the
`HumanApprover` contract. The capability validates any returned scope against
the request's offered choices before recording session grants.

The binary installs the plugin for interactive sessions. `--print` retains
unattended authorization: explicit launch grants decide which calls may run,
and other mutations are denied before human input. Its input channel carries
one prompt and closes; it has no human answer source and no approval plugin.

The shell receipt is bound before the human wait and attached only after an
allow decision. The activity block shows "awaiting approval" during the wait,
and the background slot refuses detachment until approval finishes. An
interrupt resolves the pending request before the next input receive. Human
answers are recorded as `human_approval` trajectory decisions, with the call
identity and chosen scope or denial reason, alongside permission and tool-call
records. Confirmation metadata and denial text use the existing wire format.

Confirmed replies may include `approval_note`, an optional line of user text of
at most 1 KiB. The interactive approver reads it only after validating
`confirmed` and `approval_choice`; the note never grants or widens permission.
It is redacted with the normal output policy, recorded on the human decision,
and appended to that call's model-visible result as
`User note on approval: <note>`, including an error result if execution fails or
is interrupted. A remembered session scope does not copy the note to later
calls. Replies without this metadata keep their existing result text. Denial
notes continue to use `deny: <reason>` or `no: <reason>` text.
