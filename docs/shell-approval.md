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
