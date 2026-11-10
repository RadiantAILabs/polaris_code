# Providers, credentials and models

Polaris Code uses one active model connection at a time. `--provider` (or
`POLARIS_CODE_PROVIDER`, or `provider` in the [configuration file](configuration.md))
chooses it. Everything that differs between providers lives in
`crates/polaris_code/src/providers.rs`, so adding one is one enum variant and one
arm in each of a few `match`es.

| Provider | Needs | Models |
| --- | --- | --- |
| `chatgpt` | Browser sign-in and explicit ChatGPT plan permission | Account-specific `chatgpt/<slug>` models |
| `anthropic` (default) | `ANTHROPIC_API_KEY`, or a key saved by `polaris login` | `anthropic/<model>` |
| `openai` | `OPENAI_API_KEY` or a saved key; optionally `OPENAI_BASE_URL` | `openai/<model>` |
| `openai-chat` | `OPENAI_API_KEY` or a saved key; optionally `OPENAI_BASE_URL` | `openai-chat/<exact remote model id>` |
| `bedrock` | AWS credentials through the SDK's default chain, and a region | `aws.bedrock/<id>` |

`polaris --list-models` shows what each offers, whether its credentials are
present, and the built-in price of each model. It needs no key and starts nothing.
With `--output-format json` or `stream-json`, it emits one object with
`schema: "polaris.models.v1"` and a `providers` array.

`openai` uses the Responses API. Select `openai-chat` for Chat Completions at
an OpenAI-compatible endpoint. The latter accepts arbitrary `--model` ids
without a catalog declaration; an optional `openai-chat/` prefix is stripped
once, preserving vendor slashes and routing suffixes. It reads `OPENAI_BASE_URL`
explicitly and appends `/chat/completions`. Both use `OPENAI_API_KEY`; a local
server that requires no authentication may use a non-empty placeholder key.
See [unattended runs](unattended-runs.md) for a complete invocation, reasoning
effort, and context/output limits for models outside the built-in catalog.

## Saving credentials: `polaris login`

```bash
polaris login                    # all connection options, including masked key entry
polaris login chatgpt            # browser sign-in and plan authorization
polaris login openai             # asks for the key (the input is hidden)
printf %s "$KEY" | polaris login anthropic --with-api-key   # for scripts
polaris login bedrock            # asks for a region and an optional AWS profile
polaris login status             # which providers have credentials, and from where
polaris logout openai            # forget one provider's stored credentials
polaris logout                   # forget all of them
```

`login` saves the credentials, and makes that provider the default by writing
`provider`, `model` (and, for Bedrock, `region`) to the user configuration file. A
working directory named `login` or `logout` is reached as `./login`. These
commands read no project file and run before any configuration is loaded, so a
broken `polaris.toml` cannot stop you from fixing your credentials. They write
to standard error.

**Order of use.** A provider's key comes from the first of these that exists:

1. its environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`), which
   includes a `.env` file in the folder you launch from;
2. the key `polaris login` saved.

For Bedrock, `AWS_PROFILE`, when set, wins over the saved profile.
`login status` follows the same order and says which one a run would use. It
shows at most the last four characters of a key.

**Where it is stored.** `credentials.toml` in the user configuration directory:
`$POLARIS_CONFIG_DIR`, else `$XDG_CONFIG_HOME/polaris`, else
`~/.config/polaris`. Set `POLARIS_CONFIG_DIR` to keep separate setups, for example
`POLARIS_CONFIG_DIR=~/.polaris-work polaris`. The directory is created readable by
its owner only (`0700`) and the file is written atomically with mode `0600`.
A file that other users can read is refused with the `chmod` that fixes it, and a
symbolic link is never written through. Keys are stored as plain text, like an
`.env` file; the protection is the file mode. A malformed credentials file is
reported by path and line/column only; parser diagnostics never echo its contents.
Bedrock accepts a saved AWS profile, never a stored API key.

**What `login` does not do.** It does not check an Anthropic or OpenAI key: the
framework has no free request to check one with, and a second HTTP client would
duplicate the provider's. A wrong key is reported by the provider at the first
request. Bedrock has no key, so `login bedrock` does check: it resolves the
region and one credential through the same code every Bedrock run starts with,
and saves nothing if that fails. `logout` does not unset an environment variable
and says so when one is still set.

In a session, `/login` opens setup, `/account` shows the active connection inline, and `/model`
selects from the active connection's models. Switching preserves the conversation
and permissions.

## ChatGPT plan usage

`polaris login chatgpt` signs in through your browser and asks permission to use
your ChatGPT plan.

ChatGPT plan usage requires an eligible plan.

Credentials are stored in the owner-only `chatgpt.json` beside `credentials.toml`..

ChatGPT requests go directly to OpenAI, independently of `OPENAI_BASE_URL`.

## The startup check

Before the terminal opens or a `--print` turn starts, the provider is checked
and the run stops, with a message that says what to do, when it cannot start. The
interactive and headless paths share one function (`providers::prepare`).
Interactive launches with missing configuration open setup. Headless runs
report the error and exit without prompting.

| Provider | Stops when | The message names |
| --- | --- | --- |
| `anthropic`, `openai`, `openai-chat` | no variable and no stored key; a stored file others can read | `polaris login`, the variable, where to get a key, the other providers, `.env` |
| `openai`, `openai-chat` | `OPENAI_BASE_URL` is not an `http` or `https` address | the value (credentials removed) |
| `anthropic` | `ANTHROPIC_BASE_URL` is set | why it is refused (below) |
| `bedrock` | no region | `--region`, `AWS_REGION`, the AWS profile, `polaris login bedrock` |
| `bedrock` | the credential chain yields nothing (20 s limit) | what the SDK tried and said; `AWS_PROFILE`, `aws sso login`, access keys |

When it passes, `--print` reports credential provenance on standard error.
Interactive sessions show the active connection beside the ready/busy status,
with `/login` available to change it. `/account` and `/usage` show inline replies.

```
bedrock · region us-east-1 · using AWS credentials from EC2 instance role
openai · endpoint http://localhost:11434/v1 · using API key from OPENAI_API_KEY
anthropic · using stored API key
chatgpt · using ChatGPT account usage (personal)
```

It names where a credential came from and never the credential. The same facts
are available to other code as the `ProviderReport` global resource, which
records the launch. `/status` reads the live report of the active connection
(`ConnectionRuntime::report`) for the credential source, resolved region, and
endpoint, so it follows `/login` switches; it does not infer credentials from
current environment variables. No check makes a model call. Startup and account
errors retain their original typed causes for callers that inspect
`std::error::Error::source`.

## OpenAI and OpenAI-compatible servers

`--provider openai` uses the Responses API (`/v1/responses`). Ollama, LM Studio,
vLLM, OpenRouter and company gateways can serve it. Point `polaris` at one with
`OPENAI_BASE_URL`; the framework's OpenAI client reads that variable when the
provider is built, and `tests/openai_endpoint.rs` pins it against a loopback
server. Declare the server's models, because no list is built in for it:

```bash
export OPENAI_BASE_URL=http://localhost:11434/v1
export OPENAI_API_KEY=local        # any non-empty value, if the server checks none
export POLARIS_CODE_MODELS=openai/qwen3,openai/llama3
polaris --provider openai --model openai/qwen3
```

Things to know:

- The server must implement `/v1/responses`; chat-completions-only servers will
  not work.
- Local and gateway models have no built-in price, so `--max-cost-usd` is refused
  for them; bound a run with `--max-tokens` instead.
- `POLARIS_CODE_MAX_OUTPUT_TOKENS` is not applied: `OpenAiPlugin` has no
  registration-level output limit, as the Anthropic and Bedrock plugins have.
- The agent loop and its prompts are tuned on Claude models. Other models work
  through the same tools, with no promise of the same results.
- Topology helper models (`POLARIS_CODE_LEAN_LOCATE_MODEL` and the gate
  planner's default) are Anthropic ids; set them to a model the provider serves
  if you use those features with another provider.

## Anthropic through a gateway

Not supported yet, and refused rather than ignored. The framework's
`AnthropicPlugin` has no base-URL setter, so a run with `ANTHROPIC_BASE_URL` set
would send your key to `api.anthropic.com`, not to the gateway you named. Unset
the variable, or use `--provider openai` with a gateway that offers the OpenAI
API. The request for the setter is recorded for the framework; the refusal then
becomes a plain use of it.

## Models

Each provider has a built-in set and a default; `POLARIS_CODE_MODEL` and
`POLARIS_CODE_MODELS` (or `model` and `models` in the configuration file)
override them. `--list-models` prints the result.

| Provider | Default | Built in |
| --- | --- | --- |
| `anthropic` | `anthropic/claude-sonnet-5-5` | Opus 5.5, Sonnet 5.5, Haiku 5.5, Fable 5.1, and the earlier `claude-opus-5`, `claude-sonnet-5`, `claude-sonnet-4-6`, `claude-haiku-4-5` |
| `openai` | `openai/gpt-5.5` | `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-6-luna` |
| `bedrock` | `aws.bedrock/global.anthropic.claude-sonnet-5-5` | the `global.` profiles of Opus 5.5, Sonnet 5.5, Haiku 5.5, Fable 5.1, `claude-opus-5` and `claude-sonnet-5`; declare regional (`us.`, `eu.`, ...) profiles yourself |

The built-in Bedrock models are `global.` cross-region inference profiles, so
AWS may process your prompts in a region other than your home region. To keep
processing within a geography, declare `us.` or `eu.` profiles with
`POLARIS_CODE_MODELS`.

The OpenAI ids are the ones the framework's built-in price table lists. Every
Anthropic id is in that table except `claude-haiku-5-5`, so `--max-cost-usd` on
it needs the live price table.

**Family names.** `--model sonnet` (and `/model sonnet`) picks the newest model of
that family in the set and says so: `` `sonnet` matches 3 models; using the
newest, `anthropic/claude-sonnet-5-5` ``. Versions are read from the id
(`claude-sonnet-5-5` is newer than `claude-sonnet-5`, which is newer than
`claude-sonnet-4-6`), and a snapshot date is ignored. A name that spans several
families (`claude`), or variants rather than versions (`gpt` matches both `gpt-5.4`
and `gpt-5.4-mini`), is still refused as ambiguous, listing the candidates.

**`/model` and new sessions.** A `/model` choice stays for the life of the
process: `/new` and `/resume` reset the permission mode to `confirm`, as before,
but keep the model.

## Failure messages

Provider names and authentication/network hints come from `Provider`, matched
against the model registry prefix (including `aws.bedrock/`). An authentication
failure for an API-key provider points to both `polaris login` and its key
variable, because either may have supplied the rejected key.

Bedrock retries are classified from the modeled SDK error retained in the
provider error's source. Converse, ConverseStream, and mid-stream event errors
use their modeled exception variants: model timeouts are timeouts, and service
unavailability, internal server errors, and models not ready are transient.
Unknown errors stay non-retryable even when their diagnostic text mentions a
transient exception. Other providers continue to use typed generation errors,
HTTP status, and structured error types. The retry policy consumes the resulting
stable `FailureKind` values.
