---
title: MCP servers
---

# MCP servers

Polaris connects to Model Context Protocol servers over stdio or Streamable
HTTP. Servers add tools named `mcp__<server>__<tool>` to the normal tool registry.
Type `/mcp` to see configured servers, connection status, and discovered tools.
The command runs locally without a model request. Embedders read the same
snapshot from `McpRegistry::servers()`; `McpServerStatus::status` is a `McpStatus`
value (`Connected`, `Disconnected`, `Disabled`, or `Failed(reason)`). Displaying
that value produces the same text as `/mcp`.

## Configuration

Put project servers in `.mcp.json` at the configured working-directory root.
User servers live in `$POLARIS_CONFIG_DIR/mcp.json` when `POLARIS_CONFIG_DIR`
is set and nonempty. Otherwise they use `$XDG_CONFIG_HOME/polaris/mcp.json`,
then `$HOME/.config/polaris/mcp.json`. Select an independent setup per launch
with, for example, `POLARIS_CONFIG_DIR=~/.polaris-work polaris`.
Both files use a `mcpServers` map:

```json
{
  "mcpServers": {
    "docs": {
      "command": "/usr/bin/python3",
      "args": ["/opt/docs-server/server.py"],
      "env": {"DOCS_ROOT": "/srv/docs"},
      "readOnly": true,
      "timeoutSecs": 30
    },
    "remote": {
      "url": "https://example.org/mcp",
      "timeoutSecs": 30
    }
  }
}
```

Set exactly one `command` or `url`. Commands execute directly, with no shell
expansion; relative arguments resolve from the configured working directory.
`env` values are literal explicit values; host-variable interpolation is not
performed. HTTP uses Streamable HTTP; legacy SSE and OAuth flows are not
supported. HTTP server declarations do not currently support custom headers.

`timeoutSecs` defaults to 30 and must be between 1 and 300. It bounds
initialization, the complete discovery operation, and every tool call.
Set `disabled: true` to keep a declaration without connecting.
A project server never replaces a user server of the same name: the user's
declaration is kept and the project's is reported as ignored.
Missing files are harmless; invalid JSON and connection failures appear in
`/mcp` without preventing other servers from starting.

Server and tool names may contain ASCII letters, digits, single underscores,
and hyphens. Underscores must be between other characters; double underscores
are reserved for namespace separators. These constraints keep server/tool
pairs distinct even where components meet a separator. The
complete tool name must fit within 64 bytes. Invalid names and duplicates reject
the server's discovery, preventing accidental grant aliases. Configure up to
32 servers. Discovery permits up to 256 tools per server and 32 pages, rejects
repeated cursors, and limits serialized tool metadata (including schemas and
descriptions) to 1 MiB across all pages. No partial catalog is registered when
a limit is exceeded. Discovery happens once at startup. Restart Polaris after
changing configuration or a server's tools.

Configuration files are capped at 1 MiB and must be regular files; a pipe or
device is refused without being read. Symlinks must resolve inside their
configuration directory (the project root or the user `polaris` directory).

## Project servers and trust

A project's `.mcp.json` is repository content, and a stdio server is a program
that starts on your account. So Polaris starts a project server only after you
have trusted its exact declaration. Your own `mcp.json` is always trusted.

A project server that is not trusted is never started or connected to, in any
permission mode and in unattended runs. `/mcp` lists it as
`not started: project server not trusted`, and Polaris prints one line naming
the held-back servers at startup: in the transcript for a session, on stderr for
`--print`.

To decide, run `/mcp trust` in a session. It shows what each held-back server
would run (the command and arguments, or the address, and the names of its
environment variables but not their values). Then run `/mcp trust <server>` for
each one you accept. The decision is saved in your user configuration directory
(`project-trust.json`) for that project, and the server starts the next time
Polaris starts, including in later `--print` runs. An unattended run cannot
trust a server, and `--allow-tool` grants do not start one.

The decision covers the declaration as it was when you trusted it. Changing the
command, arguments, environment, address or timeout makes the server untrusted
again until you trust the new version. Trust given in one project does not apply
to another.
If the trust file cannot be read or written, nothing is trusted. Up to 32
held-back servers are listed; more are counted but not listed or started.

`readOnly` is a statement by the operator that a server's tools do not change
anything, so only your own `mcp.json` can make it. A project's `readOnly` is
ignored and reported, even for a server you trust.

## Permissions and trust

Every MCP tool registers as `ToolPermission::Confirm`, and the MCP plugin
declares in the `ToolPolicyRegistry` that every attended call needs a person.
Every MCP call requires confirmation in `confirm`, `auto-edit` and `auto`. In
`auto` the automatic reviewer never sees an MCP call, and neither a permission
rule that allows it nor a session grant removes the prompt.
In `plan`, calls run only when the operator has declared the server
`readOnly: true`; otherwise they are denied. Treat that declaration as a trust decision: remote
`readOnlyHint` annotations never grant permission.
Unattended `--print` calls require an explicit run-scoped grant for the
namespaced tool; file-edit and shell grants do not grant MCP access.
Explicit denials take precedence over grants. The full table is in
[`permission-modes.md`](permission-modes.md#tools-that-need-a-person).

Grant each MCP tool by its exact name:

```bash
polaris --print 'Search the documentation for this API' --allow-tool mcp__docs__search
```

Generic `*` grants never match MCP tools; wildcard MCP grants are rejected.

Review server declarations before you trust them: a trusted stdio command
starts during discovery, before any tool call. Server processes and remote
services are external programs. A working directory and a cleared environment
do not provide an operating-system filesystem or network sandbox. Run servers
with the permissions and service credentials appropriate to their purpose.
MCP side effects do not participate in the local edit journal, index refresh,
or `/rewind`; review local changes independently after approving a server that
can edit files.

Stdio children receive a cleared environment with a fixed
`PATH=/usr/local/bin:/usr/bin:/bin`, the shell tool's small base
(`HOME`, `USER`, `LOGNAME`, `SHELL`, `LANG`, `TERM`, `TMPDIR`), and the explicitly
configured `env` map. Host API keys and cloud environment credentials are
not inherited. Explicit credentials should stay out of committed project files.
Server stderr is discarded to keep it out of the TUI and diagnostic logs.

HTTP JSON responses, including initialization, discovery, tool results, and
error bodies, are limited to 1 MiB before parsing. A larger advertised length is
rejected immediately; streamed and chunked bodies are stopped when they exceed
the same limit. Each POST SSE response has a 1 MiB total raw-byte budget;
standalone GET SSE streams use the SDK's 1 MiB raw-event limit. Stdio retains
the SDK's 16 MiB message limit. These transport bounds are separate from the
smaller displayed-result budget.

Tool results preserve MCP content and structured content as JSON text, capped
at the existing file-read budget (32 KiB, with a truncation marker). They enter
the normal tool-result secret-redaction, transcript, trajectory, and context
budget paths. MCP error results become tool errors; protocol errors use bounded
diagnostics without raw server error messages.

A timed-out or cancelled call closes its connection and terminates its stdio
process group (a job object on Windows). `/mcp` then reports it as disconnected;
restart to reconnect.
Disconnecting an HTTP call cannot guarantee cancellation of remote side effects.
Plugin cleanup closes all connections, with a five-second shutdown bound per
server.
