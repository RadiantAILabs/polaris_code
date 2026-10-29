# Secret redaction

`polaris-code` withholds recognisable credentials from tool output before that
output reaches the model, the transcript, the log or the session graph.

## This is not a permission boundary

Redaction is an **accident** control. It reduces what an agent doing ordinary
work — reading a `.env`, printing an environment, tailing a log, pasting a
failing curl — leaks to a model provider and to a session file on disk.

It does **not** contain a model that is trying to read a secret out. A model can
ask a tool to transform the bytes first (`base64 .env`, `rev`, `fold -w1`,
`grep -o .`), and any such transform defeats every rule below. Nothing here is a
substitute for the permission model, for not putting credentials where the
sandbox can read them, or for rotating a key that an agent has seen.

Do not describe this feature as containment, isolation, or a security boundary.
The permission model is documented separately in
[`permission-modes.md`](permission-modes.md); that is the boundary.

## Where it applies

The seam is the point where a tool result becomes a string, in
`crates/polaris_code/src/agents/code_agent.rs`: `execute_tools` for a
sequential call and `execute_read_only_batch` for a batch of reads run
concurrently. Both apply [`SecretRedactor`] to every tool result — success
bodies, error strings, and each live-streamed chunk — *before* the result is
turned into a canonical message. Everything downstream of that point reads the
redacted text:

| Consumer | Reached via |
|----------|-------------|
| The model provider | the `ToolResult` block in the next `LlmRequest` |
| The persisted session | the same canonical message, serialised by the `FileStore` |
| The session `ABox` | `record_tool_call`'s reified `ToolCall` detail |
| The terminal / `--print` output | the `UserIO` tool-activity message |
| The log | `tracing::info!` on the tool result |

`tests/secret_redaction.rs` drives one real turn through the binary's own plugin
set and asserts all four landing places are clear of a secret the tool read.

## What is covered

**Provider key formats**, recognised by literal prefix plus a bounded body
length and character class — never by entropy (see below):

| Kind | Prefixes |
|------|----------|
| `anthropic-api-key` | `sk-ant-` |
| `openai-api-key` | `sk-proj-`, `sk-` |
| `aws-access-key-id` | `AKIA` `ASIA` `AROA` `AIDA` `AGPA` `AIPA` `ANPA` `ANVA` `ABIA` |
| `google-api-key` | `AIza` |
| `github-token` | `ghp_` `gho_` `ghu_` `ghs_` `ghr_`, `github_pat_` |
| `gitlab-token` | `glpat-` |
| `slack-token` | `xoxb-` `xoxa-` `xoxp-` `xoxr-` `xoxs-` `xoxe-` |
| `stripe-secret-key` | `sk_live_`, `rk_live_` |
| `npm-token` | `npm_` |
| `hugging-face-token` | `hf_` |
| `pypi-token` | `pypi-AgEIcHlwaS5vcmc` |

A run whose length falls outside the shape's bounds is *not* that shape, so
`sk-ant-` in prose and a truncated key in a log line are both left alone. The
two prefixes generic enough to collide with ordinary kebab-case English (`sk-`,
`xox*-`) additionally require at least one digit in the body.

**Values bound to a secret-marking name** — `KEY=value`, `key: value`,
`"key": "value"` — where the name's last word is one of `password`, `passwd`,
`pwd`, `secret`, `token`, `credential(s)`, `passphrase`, `apikey`, or its last
two words are `api key`, `access key`, `secret key`, `private key`,
`signing key`, `encryption key`, `auth token`. Names are split on `_ - . space`
and camelCase, and matched on whole words only, so `sort_key`, `cache_key`,
`public_key`, `secret_name` and `API_KEY_VAR` are untouched. `key` alone is
never a secret word. Anything containing a `tokeniz*` word is excluded outright.

An all-caps name (the environment-variable shape) accepts a bare value of four
characters or more. A lower-case name has to clear a higher bar — a quoted value
of eight characters with no whitespace, or a bare value of sixteen characters
containing a digit — because `password: Option<String>` is a struct field, not a
credential.

**Private key blocks**: everything between `-----BEGIN … PRIVATE KEY-----` and
the matching `-----END`, newline-agnostic, and to the end of the input when the
block is unterminated. `-----BEGIN CERTIFICATE-----` is public and is left
alone.

**JSON Web Tokens**: three base64url segments of at least ten characters each,
where the header segment starts `eyJ`.

**Credential headers**: `authorization`, `proxy-authorization`, `x-api-key`,
`x-auth-token`, `x-amz-security-token`, recognised in curl trace output
(`> `, `< `, `* `) and in `-H "…"` / `--header '…'` arguments. The auth scheme
word (`Bearer`, `Basic`, `Digest`, `Token`, `Negotiate`) is kept so the reader
still knows what kind of credential was withheld.

**URL passwords**: the password in `scheme://user:password@host`. The scheme,
user and host survive.

## What is deliberately not covered

Redaction that eats a tool result the model needs is worse than no redaction at
all, so every rule above is narrow by choice. Known, accepted gaps:

- **No entropy detection.** Deliberate and non-negotiable: "this looks random"
  fires on minified JavaScript, base64 payloads, hashes, UUIDs, lockfile
  checksums and compiler output — exactly the content a coding agent reads all
  day. Every rule here is shape-based.
- **Unrecognised providers.** A credential with a prefix not in the table, or a
  bare high-entropy string with no name and no shape (`curl -d
  'a9f3…'`), passes through. The table is a closed list; extending it is a code
  change.
- **Weak lower-case assignments.** An unquoted lower-case `secret: abc123` under
  sixteen characters, or one with no digit, is not withheld — the false-positive
  cost of that rule is higher than its value.
- **Multi-word bare values.** A bare value stops at the first whitespace, so
  `ANTHROPIC_API_KEY=… cargo run` keeps the command but a passphrase with spaces
  survives past its first word.
- **Placeholders are intentionally preserved**: `${VAR}`, `<your-key-here>`,
  `{{key}}`, `%(key)s`, `****`, and boolean/`null` literals are never
  substituted. These are the values documentation is made of.
- **Tool arguments are not redacted**, only results. An argument was written by
  the model, so it is already in the transcript and in the provider's context
  before any rule could run.
- **Streaming is best-effort.** Live chunks are scanned individually, so a
  secret split across a chunk boundary can reach the terminal even though the
  final result is redacted.
- **Cookies** (`Set-Cookie`, `Cookie`) are not covered.
- **The model's own output** is not scanned. If a model echoes a secret it read
  before redaction was enabled, nothing rewrites it.

## What a user sees

Nothing is silently substituted. Three signals, every time:

1. The value becomes a visible, greppable marker naming what it was:
   `[REDACTED:anthropic-api-key]`, `[REDACTED:private-key]`,
   `[REDACTED:secret-assignment]`.
2. The tool result gains a trailing line —
   `[polaris-code redacted 3 secret-shaped value(s) from this result:
   anthropic-api-key, aws-access-key-id, secret-assignment]` — so the model is
   told the bytes are not the bytes on disk rather than left to infer it.
3. A `tracing::warn!` records the tool name, the count and the kinds. It never
   records the value.

## Turning it off

Enabled by default; a host has to opt out.

```bash
POLARIS_CODE_REDACT=0 polaris-code      # also: false, off
```

The escape hatch exists because a rule that eats a result the model needs is
worse than no rule at all — if redaction is blocking real work, turn it off for
that run and report the false positive. Disabled, `SecretRedactor` is a
pass-through.

## Relationship to upstream `RedactionRules`

`polaris-ai` ships `RedactionRules` in `polaris_core_plugins::inspection`. It is
**not** what implements this feature, and it cannot be: it matches on a record's
*metadata* — the binding name (`ParamMeta::param`) and the declared type
(`ParamMeta::type_name`) — and by its own documentation "never on the value". It
withholds a whole parameter from the ECS inspection recording pipeline; it
cannot see inside a tool-result string, which is where a secret a tool read
actually lives. `polaris-code` also does not install `InspectionPlugin`, so
there is no recording channel for those rules to govern.

Content-shaped redaction of a tool result is therefore a host concern today.
Making it a framework one — a value-level rule set on the tool-result path that
every Polaris host inherits — is an upstream design change and is not taken
here.

[`SecretRedactor`]: ../crates/polaris_code/src/agents/code_agent.rs
