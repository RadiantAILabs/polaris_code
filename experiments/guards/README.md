# Coding-agent guard experiments

`polaris-guards` is an unpublished host for `polaris_code`. It compares six
ways of enforcing the same rules without copying the agent graph, tool loop,
or startup. With neither `--rules-arm` nor `--max-messages`, it runs the stock
agent. All stock flags, providers, configuration files and permission controls
remain available.

Build and run from the workspace root:

```sh
cargo +1.97.0 build -p polaris-guards
cargo +1.97.0 run -p polaris-guards -- /path/to/project -p 'Complete the task.' --allow-edit
```

Choose a setup by adding one of the following to that command. File edits and
shell commands still need the stock unattended grants (`--allow-edit`,
`--allow-command PATTERN`, or the explicit disposable-container posture).

| Setup arguments | Mechanisms |
|---|---|
| `--rules-arm none` | Stock prompt and loop; record only. |
| `--rules-arm prompt` | Rules appended to the system prompt. |
| `--rules-arm per-call --protect test.py` | Prompt plus a code permission rule checking each file/shell call. |
| `--rules-arm steps --protect test.py --test-command 'python3 test.py'` | Prompt, call checks, protected-file restoration, and sealed completion gate. |
| `--rules-arm moved --protect test.py --test-command 'python3 test.py'` | No rules in the prompt; whole-file call checks and tree observation quote the rules when they act. |
| `--rules-arm prompt-gate --protect test.py --test-command 'python3 test.py'` | Prompt and sealed completion gate, without call checks, restoration, or solution scanning. |

`a0` through `a5` are aliases, case-insensitive. Setups require `--print`.
The give-up route is a final answer starting with `CANNOT_COMPLETE:`. The
full mechanisms and their limitations are described in [rules-arms.md](docs/rules-arms.md).

| Flag | Environment | Default / purpose |
|---|---|---|
| `--rules-arm ARM` | `POLARIS_CODE_RULES_ARM` | Unset: stock agent. |
| `--protect PATH` | `POLARIS_CODE_PROTECT` | Repeatable; environment accepts commas. Required for call-check and gate setups. |
| `--test-command COMMAND` | `POLARIS_CODE_TEST_COMMAND` | Required for `steps`, `moved`, and `prompt-gate`. Every configured arm also records an independent final check when supplied. |
| `--run-record FILE` | `POLARIS_CODE_RUN_RECORD` | No file by default; in-memory observations still support the gate. |
| `--check-timeout-secs N` | `POLARIS_CODE_CHECK_TIMEOUT_SECS` | 120 seconds per sealed check. |
| `--max-holds N` | `POLARIS_CODE_MAX_HOLDS` | 3 send-backs, ending `not_done` on failure N+1. |
| `--no-give-up` | — | Disable marker recognition and its advertised route. |
| `--rules-file FILE` | — | Replace rule words verbatim with UTF-8 file contents. |
| `--max-messages N` | — | Unset: unlimited conversation messages. Works independently of an arm. |

The original `POLARIS_CODE_EXP_REASONING_EFFORT` variable accepts `low`,
`medium`, or `high` (trimming whitespace) and ignores other values. It is translated to the current general provider setting. The original
`POLARIS_CODE_EXP_MODEL_TIMEOUT_SECS` accepts 1–86400 seconds for Bedrock;
blank values and other providers leave its behavior unchanged. Stock general
provider flags and variables remain available and take precedence.

The independent `MessageLimitPlugin` counts one system message, the task,
each assistant reply, each tool result (even when grouped), and each standalone
user/graph message. Embedded notices add no message. An append may reach N;
another model request needs fewer than N. Excess appends are rejected; effects
of a tool batch already executed remain. Context projection and compaction
never replenish this canonical budget. A completed answer at exactly N may
finish. A limit ends with process exit 1 and record reason `limits`. No budget
is imposed unless explicitly requested. An arm or message budget disables
prompt auto-routing so an ordinary prompt uses this coding loop.

The JSON record retains the original schema-1 shape: arm and
mechanism identities, input settings and prompt hash, billed model attempts,
tool calls and refusals, sealed checks, gate/restoration/tree findings,
protected-file integrity, model and reported final messages, give-up reason,
end reason, process exit code and usage. It is atomically replaced on events
and finalized after the turn, including errors and limits. A killed run leaves
`unfinished`. No record file is written unless its path is explicitly supplied.
Stock exit codes remain in effect: 0 completed (including `gave_up` and
`not_done`), 1 turn failure or message limit, 2 unattended setup error, 3 spending
ceiling, and 130 interrupted.

Each mechanism is a plugin:

| Plugin | Public seam |
|---|---|
| `RulesStatePlugin` | Typed global `RunRules` and local `StepState`; seals before provider startup. |
| `PromptRulesPlugin` | `CodeAgentSteps::configure_session` appends `AgentConfig::prompt_sections`. |
| `PerCallRulesPlugin` | `PermissionRules::add_code_rule`; an after-tools step records actual denials without rewriting results. |
| `RuleStepsPlugin` | Ordered after-tools and before-finish systems; `FinishGate::hold` retains the candidate and sends feedback. |
| `MessageLimitPlugin` | Session setup configures the existing `ContextManager` canonical limit. |
| `RunRecordPlugin` | Optional model-attempt and completed-turn observers; the latter projects the checked report. |

`RulesPlugin` selects this plugin group. The scanners and checks remain plain
logic under `src/rules/`; whole-file edit checks use the public pure edit planner
the stock tool uses. Restoration and test copies are trusted host behavior,
confined to the sealed workspace, and do not change tool permission grants.

Known differences from the original integrated binary:

- Code-rule refusals retain the stock permission wrapper:
  `Permission denied: tool '…' is not allowed (code rule "rules.integrity": …)`.
  The original refusal and configured rule words remain inside it. Canonical
  tool results and recorded result excerpts retain that wrapper.
- Stock tool definitions for `edit_file`, `glob`, and `search` have changed
  wording. The stock system prompt otherwise matched in scripted comparisons.
- `--max-messages` can also be used without a rules arm.
- For the 50 scripted comparison cases, all model-call counts matched; 42 cases
  agreed completely. Eight cases differed only in refusal framing and result
  excerpts. Refusal codes, gate decisions, check results, restoration, findings,
  end reasons, exit codes, and other record fields matched. The comparator's
  old-prefix detector labels wrapped refusals differently.

Run tests with `cargo +1.97.0 test -p polaris-guards`. They include the ported
check tests, scripted models for every setup, CLI validation, an independent
budget, and a real binary run against a local scripted endpoint with no
experiment flags.

For offline replay:

```sh
python3 experiments/guards/scripts/corpus_to_replay.py CORPUS_DIRECTORY input.json
mkdir -p scratch
cargo +1.97.0 run -p polaris-guards --example replay_checks -- input.json output.json scratch
```

The converter accepts the authored corpus format (`task-data.json`, items and
verification evidence). It reconstructs the known literal shell templates
without executing any candidate code or shell command, and refuses unknown
forms. The example replays per-call checks and the final tree scan; it does not
execute shell calls between checks. Both 144-run corpora matched the original
example item by item: 214 calls, 102 call stops, 72/72 effective cheats caught,
12/12 unsuccessful attempts caught, and zero false alarms among 60 honest cases
in each corpus.
