# Startup configuration

Polaris reads optional user and project TOML files before it creates a session:

- User: `config.toml` in the directory selected by `POLARIS_CONFIG_DIR`. When
  unset or empty, the directory falls back to `$XDG_CONFIG_HOME/polaris`, then
  `~/.config/polaris`. Empty `XDG_CONFIG_HOME` also counts as unset.
- Project: `polaris.toml` in the selected working directory (the positional
  `WORKING_DIR`, or the launch directory when omitted).

All user-level files share this directory: `config.toml` (including profiles),
`credentials.toml`, `permissions.toml`, and `mcp.json`. Select an independent setup
per launch, for example `POLARIS_CONFIG_DIR=~/.polaris-work polaris`. The directory
is used directly; Polaris does not append another `polaris` component. Per-workspace
session state stays in the workspace.

Precedence is flags, environment, project file, user file, then built-in defaults.
Unknown keys and invalid values stop startup with the file path and key, even
when another input would override them. Missing optional files are ignored;
unreadable or malformed files are errors. Each file must be a regular file of at
most 1 MiB; a larger file, a pipe, or a device is an error and is not read.
Experiment switches such as `EXP_*`, `LEAN_*`, and `TWM_*` remain
environment-only. Permission rule files are separate; `polaris.toml` does not
accept a `[permissions]` section.

The accepted keys are `provider`, `model`, `models`, `region`, `permission_mode`,
`permission_ceiling`, `max_cost_usd`, `max_tokens`, `max_turns`, and
`max_output_tokens`, in the file's base table and in each `[profiles.<name>]`
table. Anything else is an unknown key. Credentials are never accepted.

```toml
provider = "anthropic"
model = "anthropic/claude-sonnet-5"
models = ["anthropic/claude-sonnet-5", "anthropic/claude-haiku-4-5"]
permission_mode = "confirm"
permission_ceiling = "confirm"
max_turns = 30
max_output_tokens = 16000
max_tokens = 100000
max_cost_usd = 5.0

[profiles.inspect]
permission_mode = "plan"
max_turns = 10
```

Select a profile with `polaris --profile inspect`. Each file's selected profile
is applied over that file's base settings, then the project settings override
the user settings. A profile may exist in either file or both; a name absent
from both is an error. Flags and environment still take precedence.

| File key | Flag | Environment |
| --- | --- | --- |
| `provider` | `--provider` | `POLARIS_CODE_PROVIDER` |
| `model` | `--model` | `POLARIS_CODE_MODEL` |
| `models` | — | `POLARIS_CODE_MODELS` |
| `region` | `--region` | `POLARIS_CODE_REGION` |
| `permission_mode` | `--permission-mode` | `POLARIS_CODE_PERMISSION_MODE` |
| `permission_ceiling` | `--permission-ceiling` | `POLARIS_CODE_PERMISSION_CEILING` |
| `max_cost_usd` | `--max-cost-usd` | — (see below) |
| `max_tokens` | `--max-tokens` | `POLARIS_CODE_MAX_TOKENS` |
| `max_turns` | `--max-turns` | `POLARIS_CODE_MAX_TURNS` |
| `max_output_tokens` | `--max-output-tokens` | `POLARIS_CODE_MAX_OUTPUT_TOKENS` |

`models` accepts an array of model IDs or the existing comma/whitespace-separated
string format. Token and iteration limits must be positive integers. The cost
ceiling must be positive and finite; a cost-limited run also needs a priced model.
`region` applies to Bedrock; without it the AWS SDK resolves its region normally.
Permission modes are `confirm`, `auto-edit`, `auto`, or `plan`. An interactive
mode above the configured ceiling is refused. Permission mode and ceiling
settings are interactive-only and are refused for unattended runs, including when
set in a file.

Two settings are not file keys. `--notify` (or `POLARIS_CODE_NOTIFY`) chooses
when the terminal bell and window title ask for attention: `off`, `approval`
(the default; an approval is waiting), or `all` (also when a turn ends).
Non-interactive runs ignore it. `--review-model` (or `POLARIS_CODE_REVIEW_MODEL`)
names the model for automatic review, in `provider/model` form; without it the
reviewer uses the session model.

## Project files and trust

`polaris.toml` is part of the repository, so it can ask for less caution than
you chose. Polaris ignores a project permission setting that is more permissive
than your own: a `permission_mode` above what your mode and ceiling allow (your
mode is the built-in default when you set none), or a `permission_ceiling` above
yours. This covers the file's base table and the selected profile. A project may
always be stricter than you, and its other settings always apply.

Each ignored key is reported at startup (in the transcript for a session, on
stderr for `--print`). To accept them, read the file, run
`/permissions trust-project` in a session, and restart. The decision is saved in
your user configuration directory (`project-trust.json`) for this project, and it
covers the file as it was then: any later change to `polaris.toml` asks again.
Trust in one project does not apply to another. If the trust file cannot be read
or written, nothing is trusted. Unattended runs never trust a project on their
own. The same `/permissions trust-project` command also enables the project's
permission rule file; see [permission modes](permission-modes.md). Project MCP
servers are trusted separately, one at a time; see [MCP servers](mcp.md).

The existing `POLARIS_CODE_MAX_COST_USD` variable remains an independent
**per-task** limit, where zero disables it. It does not override the file's
`max_cost_usd`, which sets the **whole-run** limit from `--max-cost-usd`.
See [run cost ceilings](run-cost-ceiling.md) for enforcement details.

# Environment files and credential sources

Only `<launch directory>/.env` is loaded. Parent directories and a separately
selected working directory are never searched. This replaces the previous
implicit parent-directory search, so launching from a child directory can no
longer silently pick up credentials from a parent. When loaded, the file's path
is reported once on stderr. Existing process environment values win over `.env`
entries. `.env` values then participate in the environment precedence above,
including provider credentials and the AWS SDK's credential chain.

The launch directory belongs to the project, so its `.env` may not set the
variables that decide where your own configuration is read from or how a session
opens: `POLARIS_CONFIG_DIR`, `XDG_CONFIG_HOME`, `HOME`,
`POLARIS_CODE_PERMISSION_MODE`, and `POLARIS_CODE_PERMISSION_CEILING`. Polaris
stops at startup and names the variable. Set these in your shell or in your user
configuration instead. A value already in the process environment is not
affected. The file is also limited to 1 MiB and must be a regular file.

Credentials are not configuration-file keys. Keep them in process environment,
the launch directory's `.env`, the provider's normal credential store, or the
file `polaris login` writes (see [providers](providers.md)); a variable wins over
that file.

`/status` runs locally without a model request. It shows the application version,
provider, current session model, configured region, permission mode and ceiling,
credential source name, resolved user configuration directory and whether it came
from `POLARIS_CONFIG_DIR`, loaded configuration files and profile, environment file,
current session ID, and workspace. It never prints a credential value. AWS
credentials are identified as the SDK's default chain; an unresolved SDK region
is labelled as unresolved rather than guessed. Session navigation and `/model`
or `/permission` changes are read from the current shared session state.

Account setup can save validated defaults into the user file without rewriting
project configuration. Updates preserve existing comments and named profiles and
replace the file atomically. A symlink at the destination is refused; edit its
target directly when managing configuration through dotfile symlinks.
