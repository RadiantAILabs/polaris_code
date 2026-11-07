# Agent safety

This guide is for contributors. It covers what the model may do through
polaris_code, and what happens when the model is wrong or is misled. Ordinary
code security (secrets in code, dependencies, `unsafe`) is in
[`CONTRIBUTING.md`](../CONTRIBUTING.md#security).

Each control below has its own document. This guide states the threat model,
lists the controls in order, and links to each document. It does not repeat
their details.

## The main rule: the model is untrusted

Treat every tool call as if a stranger wrote it. Two reasons:

- **The model can be wrong.** It can pick the wrong file, write a broken edit,
  or run a command that does more than it meant.
- **Text the model reads can steer it (prompt injection).** Any text that
  reaches the model can contain instructions. Sources include:
  - files in the project, and their comments;
  - `AGENTS.md` or `CLAUDE.md` in the user's repository, which go into the
    system prompt ([`project-instructions.md`](project-instructions.md));
  - web pages read with `web_fetch` ([`web-access.md`](web-access.md));
  - output of shell commands and other tools;
  - MCP servers: their tool names, descriptions and results
    ([`mcp.md`](mcp.md)).

So a control must hold even when the model asks for the wrong thing on
purpose. A control that depends on the model following an instruction in the
prompt is not a control.

## The controls, in order

A tool call passes these checks in this order. A refusal at any step ends the
call. The order of the permission checks is in
[`permission-modes.md`](permission-modes.md).

### 1. File sandbox (working-directory containment)

- **Protects against:** file tools (`read_file`, `edit_file`, `create_file`,
  `search`, `glob`) reaching paths outside the working directory, through `..`,
  absolute paths or symlinks. `CodeToolsConfig::resolve_path` in
  [`tools/code.rs`](../crates/polaris_code/src/tools/code.rs) canonicalizes
  the path and refuses it with `escapes sandbox`. `@` mentions of outside paths
  are refused too (`tests/mention_containment.rs`).
- **Does not protect against:** reading secrets inside the working directory,
  such as `.env`. Read tools run without asking in every mode. It does not
  limit shell commands: the shell only checks the directory a command starts
  in, not the files the command touches.

### 2. Shell deny list

- **Protects against:** a few destructive commands, refused even after a
  person approves them. The list is `DENIED_COMMANDS` in
  [`main.rs`](../crates/polaris_code/src/main.rs). It is checked per
  subcommand. See [`shell-approval.md`](shell-approval.md).
- **Does not protect against:** anything else. The list has five literal
  patterns, and a small change in spelling avoids them. Patterns are text
  globs, not a shell parser.

### 3. Permission mode

- **Protects against:** calls the session's posture does not allow. `plan`
  refuses edits and shell commands; `confirm` asks for both; `--print` refuses
  both without an explicit grant. Permission rule files and the
  `--permission-ceiling` flag narrow it further. The code is
  `PermissionPolicy::resolve_call` in
  [`agents/policy.rs`](../crates/polaris_code/src/agents/policy.rs). See
  [`permission-modes.md`](permission-modes.md).
- **Does not protect against:** what an allowed call does. In `auto-edit`,
  `auto` and unattended runs with grants, calls run with nobody asked. A broad
  grant such as `cargo *` can run project code. Treat those modes as giving
  the agent the user's whole OS account.

### 4. Human approval

- **Protects against:** a call running that a person did not see. The prompt
  shows the full command and directory. The approval is bound to the exact
  arguments, directory, rule revision and session, and is used once
  (`ApprovalReceipt`). Web and MCP calls always need a person in attended
  modes (except MCP servers the user marked `readOnly` in `plan`). In `auto`, a reviewer model may answer some calls first; it never
  sees calls that only a person may approve. See
  [`shell-approval.md`](shell-approval.md) and
  [`agents/human_approval.rs`](../crates/polaris_code/src/agents/human_approval.rs).
- **Does not protect against:** a person approving a harmful command they did
  not read, or a session or prefix grant that covers more than they meant. The
  `auto` reviewer is a model and is advisory only.

### 5. Secret redaction

- **Protects against:** recognisable credentials in tool results reaching the
  model provider, the session file, the transcript or the log by accident.
  See [`secret-redaction.md`](secret-redaction.md).
- **Does not protect against:** a model that wants a secret. Encoding the
  bytes first (`base64 .env`) defeats it. Tool arguments and model output are
  not redacted. It is not a permission boundary.

### 6. Run cost ceiling

- **Protects against:** a run that keeps spending, for example a model stuck in
  a loop. `--max-cost-usd` and `--max-tokens` stop the run at a turn boundary
  with exit code `3`. See [`run-cost-ceiling.md`](run-cost-ceiling.md).
- **Does not protect against:** spend inside one turn; a turn in progress is
  never stopped. There is no limit unless the operator sets one.

### 7. Edit journal, session review and rewind

- **Protects against:** losing track of, or being unable to undo, edits made
  with `edit_file` and `create_file`. `/diff` shows them
  ([`session-change-review.md`](session-change-review.md)); `/rewind` restores
  them ([`session-rewind.md`](session-rewind.md)).
- **Does not protect against:** changes made any other way: shell commands,
  MCP servers, other programs, network side effects. Rewind never deletes a
  file the agent created; the file goes back to zero bytes.

## Rules for adding or changing a tool

Follow the plugin rules in [`philosophy.md`](philosophy.md) and
[`taxonomy.md`](taxonomy.md). For safety, also:

- **Declare what the tool does.** Register a tool that writes files, runs a
  process or reaches the network as `ToolPermission::Confirm`, never `Allow`.
  Declare its `ToolEffect` (`ReadOnly` or `Mutating`) in the
  `ToolPolicyRegistry` ([`tools/policy.rs`](../crates/polaris_code/src/tools/policy.rs)).
  A tool that reaches the network or an outside program also declares
  `AttendedApproval::Human`, as `web_fetch` and MCP tools do. Add it to
  `tool_is_read_only` in [`tools/mod.rs`](../crates/polaris_code/src/tools/mod.rs)
  only if it changes nothing on this machine.
- **Go through the permission policy.** Every call must reach the tool through
  the agent's authorization steps. A tool must not run a command or write a
  file by another route. A tool that can run a shell command belongs in
  `SHELL_EXECUTION_TOOLS`; a tool that writes files belongs in
  `FILE_WRITE_TOOLS`, so every mode gives it the same answer.
- **Fail closed when no approver is installed.** If a call still needs a person
  and `HumanApproval` is absent, the call is refused. Keep that true for new
  paths. An unknown or failed decision is a refusal, never an allow.
- **Model output never grants a permission or widens a scope.** No tool
  argument, tool result, file text or reviewer answer may create a grant,
  trust a project, mark a server read-only or change the sandbox root. Only an
  operator flag, a user setting or a person's answer may do that.
- **Keep results within their size budgets.** Read and MCP results are capped
  at 32 KiB, shell output at 64 KiB. Cap bytes read from files, networks and
  child processes before parsing. Follow [`tool-results.md`](tool-results.md).
- **Stay inside the sandbox.** Resolve every path through
  `CodeToolsConfig::resolve_path`. Do not open a path the model gave you
  directly.
- **Write file changes through the edit journal**, so `/diff` and `/rewind`
  see them. If a tool cannot, say so in its docs, as MCP does.
- **Test the attempt to get around the control.** Tests must try `..`,
  absolute paths, symlinks to outside, oversized input, a model-supplied
  `confirmed` argument, and a call in `plan` and in `--print` without a grant.
  Assert the specific refusal, not just `is_err()`.

## Headless (`--print`) runs

- No person can answer, so no `HumanApproval` is installed. Every call that
  would ask is refused.
- Each run states its grants on the command line: `--allow-edit`,
  `--allow-command <GLOB>` (one per pattern) and `--allow-tool <NAME>`. MCP
  tools need their exact name; wildcards never match them.
- Grants are fixed for the run. `--permission-mode` and
  `--permission-ceiling` are refused with exit code `2`.
- Deny and ask rules still apply; ask becomes deny. Project allow rules and
  project MCP servers are never trusted by a `--print` run on its own.
- A new feature must keep these rules. Do not add a headless default that runs
  something without a grant.

## Known gaps

These are true of the current code. Do not describe them as covered.

- **No OS sandbox for shell commands.** An approved or granted command can read
  and write anything the user's account can, including files outside the
  working directory.
- **The shell deny list is small and easy to avoid.** It stops accidents, not a
  determined model.
- **Project instructions need no trust step.** `AGENTS.md` and `CLAUDE.md` from
  the user's repository go into the system prompt as soon as the session
  starts. Project permission rules and project MCP servers, by contrast, wait
  for the user to trust them.
- **Tool output is not marked as untrusted to the model.** File text, command
  output, web pages and MCP results go to the model as plain tool results,
  with no label that separates data from instructions.
- **Reads inside the working directory are never asked.** A model can read
  `.env` and then try to send it out. The network routes (`web_fetch`, shell
  commands, MCP) ask a person in attended modes, but an unattended run with
  `--allow-tool web_fetch` or a broad `--allow-command` grant can send data out
  with nobody asked.
- **`plan` refuses only the file-write and shell tools by name.** Another
  mutating tool is refused in `plan` only if it declares
  `ToolEffect::Mutating` with `AttendedApproval::Human`. Otherwise its
  registered permission decides: `Confirm` asks a person, and `Allow` runs.
- **No default spend limit.** A run without `--max-cost-usd` or
  `--max-tokens` has no ceiling.
- **Rewind covers two tools only.** Shell and MCP changes cannot be undone with
  `/rewind`.

## Reviewer checklist

- [ ] A new tool that writes, runs a process or reaches the network registers
  as `Confirm` and declares its `ToolEffect` and approval in the
  `ToolPolicyRegistry`.
- [ ] Every call goes through `PermissionPolicy::resolve_call`; there is no
  side route.
- [ ] Missing approver, unknown decision, timeout and error all end in a
  refusal.
- [ ] No model text can create a grant, trust a project or server, or widen the
  sandbox.
- [ ] Paths go through `resolve_path`; sizes are capped before parsing.
- [ ] File writes go through the edit journal, or the docs say they do not.
- [ ] `--print` behavior is unchanged: nothing runs without an explicit grant.
- [ ] Tests try to get around the control and assert the specific refusal.
- [ ] The control's own document and the "Known gaps" list above are updated.
