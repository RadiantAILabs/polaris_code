# Building a task-specific plan from gates

`docs/topology-selection.md` covers two ways an accepted task's selected
registration resolves into a native graph: `CompositionPolicy::Deterministic`
picks a name out of a fixed recipe grammar, and `CompositionPolicy::Model`
asks a model to pick from that same fixed grammar. Both are choices among a
small, closed set of whole-graph shapes this crate hand-built ahead of time.

`CompositionPolicy::Gates` is a third option: rather than choosing a whole
graph, a planner assembles one from small composable blocks, sized to the
task in front of it. The plan format, the sealed block catalogue, the
materializer, and the bounds a plan must satisfy are all Cosmos's own
(`polaris_cosmos::composition`); this crate supplies the concrete blocks
(`cosmos::gate_blocks`) and the planner that decides
which of them to use and how to wire them (`cosmos::planner`).

## Planner facts

`planner::PlannerFacts` is the typed view a gate planner reads — the same
discipline `composition::CompositionFacts` already follows for the
finite-recipe chooser: registration, declared scope size, success-check kind,
objective, stated latency budget, and whether the host declared a test runner
or a model provider available. It adds further facts neither the finite-recipe
chooser needs. `is_question` is true for a read-only question rather than a
stated edit, which is what routes the scripted planner to a shape that writes
nothing. `is_survey` is true for a read-only survey — a statement with a
declared scope and a stub marker but no stated edits (`CodingTask::is_survey`)
— and today reaches only the model planner's own prompt (see below); the
scripted planner does not yet branch on it. `has_stated_edits` is true only
when the caller named at least one exact edit; a unit of work described only
by its problem statement — the ordinary case for a caller who has not
pre-computed the change — states none, and is neither a question nor a
survey, so this is the fact that actually distinguishes it. `statement` is
the problem in the requester's own words, bounded at
`planner::MAX_PLANNER_STATEMENT_CHARS` and carried as data, never
interpolated into anything but a model prompt's own text.
`PlannerFacts::from_composition_facts` builds one from the facts the host
already assembles for the finite-recipe chooser, so the two paths read
identical information about the turn.

Two further facts are computed separately in `host.rs` rather than lifted
from the finite-recipe chooser's own facts, and both take the opposite
default `effective_host_capabilities` does: a missing test runner or model
provider there means "no policy to honour, treat it as allowed" —
appropriate for a capability the finite-recipe grammar merely *reports* on.
Both facts below instead gate a plan that actually shells out to
model-authored code, so the absence of a `ShellExecutor` global resource, or
a policy that would not allow the shape of command the plan needs, must read
as unavailable, never as permissive-by-default.

`shell_available` answers whether the session's shell policy would let a
bounded model step run its own `python` scripts at all — checked against a
representative `python .polaris/...` command via
`ShellExecutor::check_permission`, requiring `ShellPermission::Allow`, not
just the resource's presence. It gates the self-check-gated shape below.

`reproduce_runner_available` answers a narrower question: not "would some
script run," but "would the session's shell policy run *this crate's own
fixed* reproduce-run command"
(`gate_blocks::reproduce::REPRODUCE_SCRIPT_PROGRAM` against
`gate_blocks::reproduce::REPRODUCE_SCRIPT_PATH`), checked the same way. It is
deliberately a value separate from `HostCapabilities::test_runner` rather
than a wider `effective_host_capabilities`: that fact is read by more than
the statement-only gate shape — it decides which topologies
`topology::probe_availability` even considers eligible, and it feeds the
finite-recipe and model composition policies for *every* task shape, not
only a statement-only one under `CompositionPolicy::Gates`. An attended
session's shell policy ordinarily allows any command once the outer posture
has allowed or confirmed the tool call, so declaring the capability there
would have made `code.test-gated-edit` newly eligible, and the stated-edit
gate shape newly test-gated, for every interactive `/topology` task in every
attended session — a much larger behaviour change than "a statement-only
task can now reproduce and repair against a real command." Keeping
`reproduce_runner_available` a separate, narrowly-read fact keeps that blast
radius to exactly the one branch it is meant for.

`shell_available` and `reproduce_runner_available` answer related but
distinct questions and are not required to agree: a host whose allow-list
matches a general `python *` pattern sets both, but one that allow-lists only
the literal reproduce command sets the second without the first. `scripted_plan`
below prefers the first when both hold — see "The scripted planner".

## Block descriptions

Every block the coding catalogue registers also carries a
`polaris_cosmos::composition::BlockDescription`
(`gate_blocks::*::register_all`'s own `.describe_block(...)` calls): a short
summary, the wired input ports it actually checks (`BlockFactory::wire_input`
— empty for every block except `code.apply-patch-intent`, whose real upstream
reads are documented in prose instead), the single output type it produces,
one example configured use, and the opaque host-defined facts a host must
already have satisfied for the block to be worth considering. Cosmos never
interprets a fact's meaning — a `FactKey` is just a stable string — so
`cosmos::block_cards` is the one place that translates this crate's typed
`PlannerFacts` fields into the `FactKey`s a description names
(`stated_edits_present`, `test_runner_available`, `model_provider_available`,
`question_task`, a declared-but-unused `survey_task`, `reproduce_runner_available`,
and `shell_available`) and back into a port/block vocabulary
(`applied_edit_port`, `model_step_artifact_port`,
`proposed_patch_intent_port`, and so on) every `describe_block` call reuses
rather than repeating a string literal. `code.model-propose-conversation` and
`code.resume-model-propose` require both `model_provider_available` and
`shell_available`, matching the shape they exist for; `code.run-check-script`
requires `shell_available` alone, the same way `code.write-reproduce-script`
requires `reproduce_runner_available`.

`block_cards::admissible_gate_blocks(catalogue, facts)` is the one query this
crate builds on it: every described block whose required facts `facts`
satisfies, in catalogue registration order. A block whose required fact is not
met — `code.apply-direct` on a statement-only task, `code.run-project-tests`
with no declared test runner, `code.model-propose` with no declared model
provider, `code.survey-tree` on a task that is not a question — is simply
absent from the result; nothing about the underlying registration changes; a
different run with different facts sees a different admissible set from the
same sealed catalogue. The model planner's own prompt (see "The model
planner" below) renders each admissible block's required fact keys too, not
only using them to filter the menu: a reader — model or human — sees *why* a
block is on the list, not just that it is.

## Small coding blocks

The coding catalogue also exposes task-specific read-only blocks alongside
the edit and model steps. `code.scan-markers` takes a strict configuration
with a literal `marker`, bounded `max_sites`, and exactly one scope: an
explicit sandbox-relative `paths` list or `root: true`. It feeds
`code.assess-sites`, then `code.answer-site-audit`, so a marker audit reaches
the ordinary coding assessment contract without changing the tree. A scan
stops once it finds `max_sites`, refuses to inspect more than 1,000 files or
any file larger than 1 MiB, and root walks do not follow symlinks.

`code.classify-request` reads the accepted `CodingTask` resource and emits an
`edit`, `answer`, or `clarify` `TaskDisposition`, restricted to the strict
`allowed_dispositions` configuration. The registered `code.task-disposition`
discriminator lets a plan `Switch` on that value. Its `clarify` branch can run
`code.request-clarification`, whose strict `max_questions` bound produces an
escalated conclusion carrying a bounded question rather than an edit.

`code.apply-patch-intent` accepts an explicit `code.patch-intent` input port.
A `WirePlan` from a `code.model-propose` block configured with
`output: patch-intent` can therefore make a patch application fresh on every
`Repeat` iteration. That configuration produces the dedicated `PatchIntent`
graph type, so another model output is rejected before execution rather than
reaching the mutating block. This is required for an otherwise
non-repeat-safe mutation inside that body; without the same-body wire, the
composition effects check refuses the plan before execution.

`code.judge-patch-intent` judges a model-proposed patch against what actually
landed on disk, the same way `code.judge-stated-edits` judges the task's own
stated edits — but a statement-only task states none, so that block has
nothing to check and a plan reaching for it would apply zero edits and
"succeed" at having changed nothing. `code.judge-patch-intent` checks the
edits the model itself proposed instead, and treats the empty case as
`Status::Issue`, never a vacuous pass: a model that was asked for a patch and
returned nothing has not satisfied the contract.

`code.run-project-tests-captured` runs the declared `ProjectTestRunner`
command exactly once, like `code.run-project-tests`, but keeps a bounded tail
of its combined stdout/stderr (`TestRunOutcome`) alongside the verdict rather
than discarding it. `code.judge-test-outcome` then projects that verdict into
the ordinary `Assessment<EditSite>` ledger a `Choose` predicate or
`code.conclude-single-pass` reads, so a repair loop gets both the judged
status and the raw failure text from one test run, not two.

`code.model-propose-conversation` is `code.model-propose` configured for
`patch-intent`, except it keeps the full `ModelConversation` its call
produced (`ProposedPatchIntent`) instead of discarding it once the artifact is
extracted. `code.resume-model-propose` is its repair-round counterpart: it
reads an upstream `ProposedPatchIntent` and `TestRunOutcome`, folds the
captured failure into the instruction the same way an ordinary repair step's
`sees_prior_failure` does, and resumes the *same* conversation
(`ModelStepExecutor::propose_conversation`'s `resume` argument) rather than
starting a fresh one — the whole point being that a repair round never has to
pay to re-derive context (re-reading the code, re-locating the site) a fresh
conversation would need to rebuild from nothing. `code.extract-patch-from-
proposal` projects a `ProposedPatchIntent` down to the plain `PatchIntent`
`code.apply-patch-intent` and `code.judge-patch-intent` already read, so
neither of those blocks needed to change to sit downstream of a
conversation-keeping propose step.

`code.run-check-script` runs the answer's own submitted `check` script — a
short, model-authored program meant to reproduce the issue, expected to fail
or print wrong output before the patch and pass after — through the same
sandboxed `run_python_script` a model-propose step's own `run_python` tool
uses (see below), writing it to the fixed, git-invisible
`.polaris/check_script.py` rather than the tool's own scratch path so the two
never collide. It reads `ShellExecutor` as a required resource: on a host
where the plan's own `shell_available` fact routed here, that resource is
present; a shell policy that refuses the specific command, or a script that
itself fails, both come back as an ordinary `Status::Issue` `TestRunOutcome`
with the refusal or failure text captured as output, never a hard error — only
a proposal with no `check` at all reaching this block *is* a hard error, since
the `code.has-check` gate exists to route around exactly that case before this
block ever runs.

`code.has-check` (`predicates::has_check`) is the `Choose` predicate that
reads whether a `ProposedPatchIntent`'s own answer included a `check` script
at all — one of the predicate family alongside `tests-passed`/
`review-approved`/`budget-left`, over a different typed value each time.
`code.check-needs-repair` (`predicates::check_needs_repair`) reads whether
the most recently run check script failed. A statement-only plan reads
`has-check` right after the model's first proposal to decide whether to take
the self-check branch or fall back to the plain `code.judge-patch-intent`
verdict, and `check-needs-repair` after running a submitted check to decide
whether to repair.

## The scripted planner

`planner::scripted_plan` is a pure function of `PlannerFacts` and the current
revision attempt: the same facts always resolve to the same shape. Eight
shapes cover the fixtures this module is proved against:

- a read-only question (`is_question`) surveys the tree, assesses what it
  found, and answers — writing nothing;
- a statement-only edit (`!has_stated_edits && !is_survey`) with no test
  runner and no usable shell policy asks the model to propose the whole
  patch, using its own bounded `read_file`/`list_dir`/`search` tools to
  locate what to change, then applies it and judges the *model's* edits —
  never the task's own stated ones, of which there are none;
- the same statement-only edit with a test runner installed repairs like the
  stated-edit shape below, except every propose step is a model call from the
  start (there is no stated edit for a first pass to apply directly), and
  every retry's propose step reads the preceding attempt's captured test
  failure (`code.run-project-tests-captured`/`code.judge-test-outcome`) via
  `ModelStepConfig::sees_prior_failure`, so it sees *why* the last attempt did
  not pass, not only that it did not;
- the same statement-only edit with no test runner but `shell_available`
  takes the self-check-gated shape instead (see below): the model
  additionally gets a `run_python` tool and may submit a `check` script
  alongside its patch, and a submitted check's own pass/fail — not just
  whether the edits landed — decides whether the turn concludes or repairs;
- failing that, the same statement-only edit with `reproduce_runner_available`
  (but not `shell_available`) writes and runs a dedicated reproduce script
  first, then repairs against it (see "Reproducing the issue" below) —
  reachable only on a host whose shell policy allows the reproduce block's
  own fixed command specifically without allowing a bounded step's scripts in
  general;
- a stated edit with a test runner applies it, runs the tests, and only
  builds a repair arm — propose a patch, apply it, re-run the tests — when
  they fail. The first attempt allows one retry inside that repair arm; every
  attempt after a refusal degrades to a single repair pass with no retry, so
  a limit that rejected the deeper shape has a genuinely shallower one to
  admit instead of an identical resubmission;
- a single-target stated edit with no test runner applies it directly and
  judges whether it landed;
- a multi-target stated edit with no test runner (`scope_size > 1`) runs the
  bounded worklist sweep over every stated site as one block instead — the
  same distinction the finite-recipe grammar already draws between its
  `direct` and `bounded-worklist` recipes. Two stated edits that differ only
  in how many sites they name are not the same plan: this is what keeps
  their digests apart.

Precedence among the statement-only-edit shapes is `test_runner_available` >
`shell_available` > `reproduce_runner_available` > plain fallback. The first
two model-authored-script shapes — self-check-gated and reproduce-gated —
were built independently to answer the same problem (a statement-only task
has no runnable oracle of its own) and are not one a simplification of the
  other: the self-check-gated shape folds proposing and reproducing into one
  conversation and repairs by resuming it, avoiding the redundant exploration a
  fresh conversation would require. `shell_available`
therefore outranks `reproduce_runner_available` in the scripted planner: on
any host whose policy is permissive enough to set both — which is every host
this crate's own eval harness runs under — the cheaper self-check-gated
shape is what actually runs, and the reproduce-gated shape stays reachable
only for a scripted plan under a narrower policy, or for the model planner,
which can still name `code.write-reproduce-script` directly wherever
`reproduce_runner_available` holds (see "Block descriptions" above).

All of the statement-only repair shapes — the test-runner-gated one above,
the self-check-gated one, and the reproduce-gated one — are hand-unrolled
nested `Choose` gates, the same shape and for the same reason
`test_gated_expression` already is (see below) — not `GateExpr::Repeat`.
Adopting `Repeat` here remains the simplification opportunity already
recorded for `test_gated_expression` itself; it was not attempted for either
shape in the same pass for the same reason, plus one the existing note does
not need to weigh: a retry step reading the *previous* attempt's captured
`TestRunOutcome` is straightforward with hand-unrolled `Choose` (each level's
propose step is simply downstream, in the ordinary graph sense, of the one
test run that precedes it), whereas `Repeat`'s own iterations would need the
native loop-retention machinery (`RetentionPolicy`) to carry that value from
one iteration to the next — real machinery this depth of repair does not need
to reach for.

## The self-check-gated shape

`self_check_gated_expression` is what a statement-only edit with no test
runner but `shell_available` resolves to. The idea: a statement-only task has
no runnable oracle of its own (no test runner, no stated edits to check
against), so `code.judge-patch-intent`'s "did the model's own edits land"
verdict is the best a plan can do — it says nothing about whether the change
actually *fixes* anything. When the session's shell policy would allow it, the
model can instead write a short script that reproduces the issue, run it
itself first (so it sees the bug actually happen rather than guessing), and
submit it alongside the patch as the answer's optional `check` field; the plan
then runs that script for real after applying the patch and treats *its*
pass/fail as the verdict, repairing on the same conversation if it still
fails.

Concretely, the plan is `Seq(propose-0, extract-0, apply-0, Choose(has-check:
with-check-subtree; without-check: judge-patch-intent), conclude)`:

- `propose-0` is `code.model-propose-conversation`, configured (via
  `statement_edit_model_step_config(..., offer_check: true)`) with the
  ordinary `read_file`/`list_dir`/`search` tools plus a fourth, `run_python` —
  a bounded tool that writes a script the model wrote to a scratch path under
  `.polaris/` (git-invisible) and runs it through the same sandboxed
  `run_python_script` the check itself later uses, with a timeout and output
  cap, so the model can see a script's real output before deciding what to
  submit. The instruction tells it plainly: only submit a `check` you actually
  ran and confirmed fails before your patch and passes after, and a failed
  check earns another attempt with its output, in the *same* conversation, so
  there's no need to re-explore the code;
- `extract-0`/`apply-0` are the same `code.extract-patch-from-proposal` /
  `code.apply-patch-intent` pair every statement-only shape uses, so applying
  the patch behaves identically whether or not a check was offered;
- `code.has-check` then reads whether the model's answer actually included a
  script — a well-behaved model asked for one might still have nothing worth
  checking, and a check is not mandatory. Without one, the plan falls back to
  the ordinary `code.judge-patch-intent` verdict — the same one the
  no-shell shape above already uses — so a plan built for this shape never
  behaves worse than the plain fallback;
- with one, `code.run-check-script` runs it and `code.judge-test-outcome`
  turns the captured pass/fail into the ordinary assessment ledger a `Choose`
  reads. A pass concludes immediately (`code.affirm-assessment`, mirroring
  every other shape's happy path). A failure enters the recipe's one repair round.
  A second round would name 19 instances and now fits the 64-block envelope,
  but is not part of this fixed recipe. The repair round's propose step is
  `code.resume-model-propose` rather than a fresh
  `code.model-propose`: it continues the *same* conversation from
  `propose-0`, with the failing check's own captured output folded in, instead of
  starting over and paying to re-read the code and re-locate the site. This is
  why the conversation is retained across the repair boundary.

`ModelStepConfig::sees_prior_failure`, `TokenBudget`, and `max_turns` are
unchanged for this shape: `statement_edit_model_step_config`'s
`STATEMENT_EDIT_TOKEN_BUDGET` (`configurable::MAX_MODEL_STEP_TOKEN_BUDGET`,
200,000) and `STATEMENT_EDIT_MAX_TURNS` (30) are reused as-is for the
check-offering variant, since exploring a real repository costs the same
whether or not a check is on offer. The check script itself never reaches
the final patch: `code.run-check-script` writes it to `.polaris/check_script.py`,
inside the agent's own git-invisible state directory, distinct from
`run_python`'s own scratch path so a tool call made while still exploring
never collides with the one made after the patch is applied.

`code.run-check-script`'s `ShellExecutor` read is a hard requirement, not an
optional one — a plan that reaches this block genuinely needs a shell to run
the check in. That is exactly why `shell_available` exists as its own fact
rather than reusing whatever `effective_host_capabilities` already reports:
gating a plan that shells out to model-authored code needs a fail-closed
probe (`host::select_and_run` checks `ShellExecutor::check_permission` against
a representative `python .polaris/...` command and requires `ShellPermission::Allow`,
not just the resource's presence), where the finite-recipe chooser's own
capability facts are content to default an *absent* policy to "allowed" for a
capability that is otherwise merely a routing signal.

## Reproducing the issue

A statement-only task's caller does not have to state a test command —
ordinarily it names none at all, since a unit of work described only by its
problem statement is exactly the caller who has not pre-computed anything
about it, including how to check it. In an earlier version, the planner's
own test-gated repair plan (`model_test_gated_expression`) was *never*
reachable for such a task:
`PlannerFacts.test_runner_available` came from `HostCapabilities.test_runner`,
which nothing ever set for a statement-only task even when the session's own
shell policy would in fact run a test command — so every statement-only run
landed on the bare, no-test shape, win or lose, and a syntactically-applied
but behaviourally wrong patch had nothing in the plan to ever catch it. The
self-check-gated shape above answers the same underlying problem for a host
whose policy is permissive enough to trigger `shell_available`; this shape
is what a narrower policy — one that allows only the reproduce block's own
fixed command, not a bounded step's scripts in general — falls back to
instead. See "The scripted planner" above for the precedence between them.

### Detecting a runnable command without a stated one

`host::reproduce_runner_available` answers a narrower question than
`HostCapabilities.test_runner`: not "did the task state a command a policy
would run," but "would the session's shell policy run *this shape's own
fixed* reproduce-run command" (`gate_blocks::reproduce::REPRODUCE_SCRIPT_PROGRAM`
run against `gate_blocks::reproduce::REPRODUCE_SCRIPT_PATH`), checked the same
way `host::effective_host_capabilities` already checks a task's own stated
`SuccessCheck::Command` — one call to the installed `ShellExecutor`'s
`check_permission`, never a heuristic guess at "does this policy generally
allow test commands."

This is a fact on `PlannerFacts` of its own
(`reproduce_runner_available`), not a widening of
`HostCapabilities.test_runner`/`effective_host_capabilities`, and that is a
deliberate, narrow choice: capability detection lives in the host path rather
than `terminal::topology`'s accept path.
`HostCapabilities.test_runner` is read by more than the statement-only gate
shape: it decides which topologies `topology::probe_availability` even
considers eligible, and it feeds the finite-recipe and model composition
policies for *every* task shape, not only a statement-only one under
`CompositionPolicy::Gates`. An attended session's shell policy ordinarily
allows any command once the outer posture has allowed or confirmed the tool
call (`ShellConfig`'s own `allowed_commands: ["*"]` for `Confirm`/`AutoEdit`/
`Plan`, see `main.rs`'s `shell_config`), so declaring the capability there
would have made `code.test-gated-edit` newly eligible, and the stated-edit
gate shape newly test-gated, for every interactive `/topology` task in every
attended session — a much larger behaviour change than "a statement-only task
can now reproduce and repair against a real command." Keeping
`reproduce_runner_available` a separate, narrowly-read fact keeps that blast
radius to exactly the one branch it is meant for.

The absent-`ShellExecutor` default is also the *opposite* of
`effective_host_capabilities`'s: that function reads no `ShellExecutor` global
as "no policy to honour" and trusts an explicit, caller-stated command, since
refusing an ask nobody governed would be the surprising choice.
`reproduce_runner_available` answers a question nobody asked — no caller
named this command — so it has no ask to trust, and a hermetic host with no
shell subsystem wired in at all must still mean *no* test runner: only an
installed `ShellExecutor` actually returning `ShellPermission::Allow`
establishes it.

### Writing and running the script

`code.write-reproduce-script` (`gate_blocks::reproduce::WriteReproduceScriptBlock`)
is the one new gate block this feature adds. It reads the `ModelStepArtifact`
an ordinary `code.model-propose` step produces when configured for
`ModelStepOutputKind::Answer` — asked, in its own instruction, to write a
small script reproducing the issue from the task's statement, exiting nonzero
while the bug is present and zero once it is not — and writes that answer
text verbatim to `REPRODUCE_SCRIPT_PATH`
(`.polaris/reproduce_issue.py`, under `state::state_dir`'s self-ignored state
directory: `git add -A`/`git diff`, the convention harnesses that grade a
patch already use, never sees it, so nothing has to delete it before a caller
reads the final diff). No new decode path, no new `ModelStepOutputKind`:
`Answer` already exists and already produces free-form text, and this is
simply its first real consumer.

Running the script reuses `code.run-project-tests-captured` and
`code.judge-test-outcome` completely unchanged: the host installs a
`ProjectTestRunner` naming `REPRODUCE_SCRIPT_PROGRAM` (`python` — fixed,
unlike the script's own content, matching the shell-policy pattern this
feature exists for) and `REPRODUCE_SCRIPT_PATH`, exactly the way it already
installs one from a task's own stated `SuccessCommand`. Neither block knows
or cares whether the command it is handed came from the task or from this
fixed path.

The full shape
(`planner::model_reproduce_gated_expression`) is: propose the script, write
it, run it once as a baseline (confirming it actually reproduces the issue,
and recording that run's own captured output and verdict as ordinary
evidence — nothing branches on it), then `model_test_gated_expression`'s own
suffix, unmodified — propose the code fix, apply it, run the *same* script
again, and repair against a still-failing run exactly as it would a stated
test command's own failure. Composed, not duplicated: the four steps above
are the only thing this shape adds. Unlike the self-check-gated shape, this
repair round starts a fresh `code.model-propose` conversation rather than
resuming one — it composes `model_test_gated_expression`'s own suffix
unmodified, and that suffix predates the resume-conversation mechanism. This
is the concrete cost difference behind the precedence in "The scripted
planner" above: reachable, real, and tested, but not the shape this crate
picks first when a host would allow either.

One constraint forced a real compromise: `MAX_COMPOSITION_BLOCKS` bounds
every gate plan at 16 block instances, and `model_test_gated_expression`'s
own deep, two-level repair tree (built for a first attempt) is already 15 of
those alone — no room for this shape's four-step prefix. So this shape always
asks for the *shallow*, single-repair-level tree (passing a fixed revision
attempt of `2`), regardless of the gate planner's own revision-loop attempt
number: `4 + 10 = 14`. This costs the reproduce shape one level of in-graph
repair depth compared to a stated edit's own first attempt — still one retry,
backed by the captured failure — never the caller's real revision loop, which
`attempt_gate_plan` still drives across whole fresh plans exactly as it does
for every other shape.

## The model planner

`planner::model_plan` mirrors `composition::choose`'s model branch: the model
names an ordered sequence of gate blocks (plus, for a model-propose step, its
own small configuration) by calling a `submit_plan` tool — the same "answer
through a tool call" shape `gate_blocks::configurable` uses for a
model-propose step's own answer. `model_plan` is one single-shot call with no
turn loop of its own: a plain-text reply is still accepted as a lenient
fallback (a ` ```json `/` ``` ` fence and surrounding prose stripped via
`extract_json_object` before parsing), but there is no in-call retry on a bad
answer — a malformed `submit_plan` argument or an unparseable fallback both
become the same `PlannerError::InvalidModelAnswer` an unrelated bad answer
already produced, which is what the revision loop below turns into the next
attempt's whole new prompt. `model_plan` itself is `model_plan_for_selection`
called with the default (generic) `domains::DomainSelection`; see "Domain
selection" below for the one place a non-default selection changes this.

The parsed answer, however it arrived, is checked against the bounded grammar
again before any catalogue key is trusted — an unknown block, predicate, or
discriminator name, a duplicate instance label, a malformed gate shape, or an
unusable configuration all refuse the same way an unapproved recipe does for
the finite-recipe chooser, never reaching `materialize`. The model never sees
or returns a native graph. As with the finite-recipe chooser, the meter is
billed unconditionally before the answer is validated, so a refused answer
stays auditable evidence rather than a silent discard of a real call. On a
revision, the prompt also states the previous plan's refusals — code, path,
expected, actual, repair hint — and asks for a whole new plan, never a patch
to the refused one.

The named block set is `planner::nameable_gate_blocks_for_selection`, sourced
from `block_cards::admissible_gate_blocks` (see "Block descriptions" above)
rather than a hand-maintained list or a hand-maintained hidden-block set: a
block the task's own facts do not satisfy is hidden from the prompt, and
naming it anyway hits the ordinary "unknown block" refusal (`unknown_block_detail`)
— the same generic mechanism covers a statement-only task's `code.apply-direct`,
a test-runner-less task's `code.run-project-tests-captured`, a task with no
shell policy's `code.model-propose-conversation`/`code.run-check-script`, and
every other fact-gated block, not just the stated-edits family; naming a
stated-edits-only block (`stated_edit_only_blocks`) for a task that stated
none also gets the actionable hint of what to submit instead
(`stated_edits_refusal_detail`), not just the bare refusal. Two further
filters are independent of `facts`: `code.model-direct-edit` and
`code.conclude-unverified` are never offered (only the `loop` topology's
hand-written plan ever names them), and every domain
expert block (`crate::cosmos::domains`) is offered only under
`domains::Arm::Composed`, and then only the selected domain's own — see
"Domain selection" below. Unlike an earlier version of this grammar, a block
registered under its own strict configuration schema (`code.create-file`'s
path/content, `code.survey-paths`'s path list, `code.scan-markers`'s
marker/max_sites/paths, `code.classify-request`'s allowed dispositions,
`code.request-clarification`'s max_questions) is offered too, not withheld
for a future grammar: a step naming one configures it as `{"registered":
value}` (`ModelGateConfigAnswer::Registered`), decoded by
`planner::model_grammar::configuration` and carried under the block's own
registered schema key, validated by the block's own factory during Cosmos
materialization — `planner::model_grammar::prompt` appends each such
schema's exact shape, the reader/producer `wires` grammar (below), and the
host's own composition limits (64 instances, depth 12, 128 wires, 2 nested
repeats, 256 native nodes) to the prompt, after the block/predicate/
discriminator listing this section describes. Each nameable block's own
prompt line is built from its `BlockDescription` — summary, declared effect
(read-only, changes files, or calls a model), what it produces, its example,
and its required fact keys (or `(requires: none)` when it has none) — not a
second, independently maintained one-liner; a domain expert block carries no
catalogue description (`crate::cosmos::domains` registers no `BlockDescription`
for them), so its one-line meaning comes from a small hand-maintained table
instead (`expert_block_meaning`). Predicates and discriminators remain a
small, hand-maintained list (`nameable_predicates`/`nameable_discriminators`):
the catalogue carries no description for them today, and (see the source doc
comments) no discriminator is actually usable through a bare submitted plan
against the coding target contract yet, so the discriminator list is empty —
and, because a shape every submission of it is refused is worse than not
offering it, the prompt does not offer `switch` at all while that list stays
empty (`planner::gate_kind_descriptions`, the same flag gating the `switch`
`<node>` clause below). The shape still parses (see the source doc comments)
and resolves against whatever `nameable_discriminators` returns, so both the
sentence and the grammar clause come back the day a discriminator is
registered, with nothing to remember to re-enable by hand. `nameable_predicates`
names `tests-passed`, `review-approved`, `budget-left`, `has-check`, and
`check-needs-repair`, the same five the coding catalogue's own predicates
module registers.

The prompt also states the task's own `statement` and every `PlannerFacts`
field a scripted plan would branch on, including `reproduce_runner_available`,
so a model choosing among gate blocks has the same information a human
reading the turn would.

**Domain selection.** `attempt_gate_plan` and `model_plan_for_selection` both
take a `domains::DomainSelection` (domain plus `Arm`); every arm but
`Arm::Composed` leaves the prompt exactly as described above. Under
`Arm::Composed`, `nameable_gate_blocks_for_selection` additionally offers the
selected domain's own expert blocks (`domains::expert_keys`) while still
hiding every *other* domain's blocks (`domains::keys`), and
`composed_context` appends one more prompt section: the selected domain's
expert notes (`domains::expert_notes`) plus two worked examples rendered with
`render_gate_expression_shape` — the plain generic plan (`scripted_plan`) and
the selected domain's own expert plan (`domains::plan_for_selection` with
`Arm::Expert`) — side by side, so the model sees a real domain-informed shape
alongside the generic one it could otherwise fall back to.
`attempt_gate_plan` refuses `Arm::Composed` outright, before any attempt, when
paired with `GatePlannerMode::Scripted`: a composed prompt is a model-planner
feature, and the scripted planner has no comparable dispatch to make on its
own facts alone (`domains::plan_for_selection`'s own `Arm::Composed` arm calls
`scripted_plan` unchanged instead, the same as `Arm::Generic`).

**The answer may be a flat list or a full gate-expression tree, either one
optionally carrying typed cross-instance wires.** `ModelGatePlanAnswer`
accepts `{"steps": [...], "grounds": "..."}` (an implicit `Seq` of leaf steps)
or `{"plan": <node>, "grounds": "..."}`, never both, never neither. `<node>`
is tagged by `"kind"` and covers every `GateExpr` shape the coding catalogue
can actually back: `step`, `seq`, `choose` (a named predicate branching
between two sub-expressions), `fork` (concurrent arms reduced by one named
block — each arm is its own self-contained subgraph, so a value produced
*outside* a `fork` is not visible *inside* one of its arms), `loop` (repeats
its body while a named predicate holds, checked before every iteration
including the first so it may run zero times, or a fixed count), and
`repeat` (like `loop`, but its body always runs once before that same check,
so it is guaranteed to run at least once). `switch` (a named discriminator's
case bodies, plus an optional default) is a real, parseable shape too, but
the prompt only offers it once `nameable_discriminators` actually names one
— see above — since none is registered today. `Guard`, `Seal`, and `Slot` are
never in the grammar at all: the coding catalogue registers no handler,
timeout policy, context policy, or selector for any of them to resolve
against, so offering the shape would only manufacture refusals a model could
never act on. A depth guard (`MAX_MODEL_PLAN_TREE_DEPTH`) stops a
pathological tree during parsing, independent of the host's own
`CompositionLimits::max_depth` (checked later, with a precise path, once the
tree is built). Every instance label — a `step`, or a `fork`'s reducer — must
be unique across the whole plan, not just within one branch. A flat `steps`
answer may instead carry an older, narrower alternative to a `plan` tree: an
optional `expression` (`planner::model_grammar::Expression`) naming each
declared instance once with `step`, `seq`, `choose`, `switch`, or bounded
`repeat` (no `fork`/`loop`/`guard`/`seal`/`slot` — `plan` is the one grammar
that reaches those). Independent of which of the two tree shapes is used (or
neither), either may carry `wires`: typed connections from a producer
instance's own output port to a reader instance that is not its immediate
successor — the case a plain sequential or branching order cannot express on
its own, most concretely a `fork` arm that needs a value produced outside it.

A `code.model-propose` step's configuration also grew: besides `role`,
`instruction`, `tools`, and `token_budget`, the model may set `output`
(`plan`, `findings`, `patch-intent`, `review`, or `answer` — optional,
defaulting to `patch-intent`, the one kind the original flat grammar always
produced, so an older plan shape naming no `output` at all still means what
it always meant), `max_turns` (bounded at build time by
`ModelStepConfig::decode`'s own ceiling, defaulting to
`STATEMENT_EDIT_MAX_TURNS` when omitted), `sees_prior_failure` (reads a
preceding captured test failure, refused by native graph validation if
nothing produces one), and `context_mode` (`fresh`, `brief`, or `continue`,
defaulting to `fresh`). A downstream block wired to an output kind it cannot
consume — `code.apply-patch-intent` fed a `review` artifact, say — is refused
by the same native graph validation an ordinary missing producer trips, not a
second admissibility check duplicated here. `offer_check`/`review_before_submit`
are not part of this model-choosable configuration surface: both remain
scripted-planner-only knobs, set by `statement_edit_model_step_config` rather
than by anything the bounded JSON grammar decodes, so a model-submitted plan
cannot ask for the self-check tool subset or the pre-submit review step.
Every model-propose instance's instruction also gets `WORKSPACE_ONLY_RULE`
appended, the same rule every scripted edit step carries, so a
model-submitted plan cannot talk a step into fetching another version of the
project.

`token_budget` is bounded too, and — unlike `max_turns` — checked twice:
`planner::build_step_answer` refuses an out-of-range value itself, before the
answer is even encoded, and `configurable::ModelStepConfig::decode` refuses it
again once the step is later built, both against the same
`configurable::MAX_MODEL_STEP_TOKEN_BUDGET` (200,000) so the two bounds cannot
drift apart. This value becomes that turn's real `LlmRequest.max_output_tokens`
verbatim (see "The model-propose block's own executor" below), so an unbounded
budget is a provider bill, not just a wasted turn — worth refusing before a
whole plan is even built, not only once materialisation gets far enough to
build the block. The ceiling itself is not a fresh number: it is exactly the
budget `planner::statement_edit_model_step_config` already gives a
statement-only edit's own open-ended repository exploration — the codebase's
own most generous built-in case — so nothing this codebase ships needs more
than that in one turn, and a model asking for more gets a refusal it can
revise, the same as any other out-of-grammar answer.

**Worked examples.** The prompt ends with four complete, valid answers, each
covering a shape the scripted planner also builds so a model sees the grammar
used the way this crate itself uses it: a stated edit with no test runner (the
flat legacy grammar); a statement-only edit with a test runner (propose,
apply, test; on failure, re-propose once more seeing the captured failure,
then conclude either way); a statement-only edit with no test runner (the
model investigates with its own bounded tools, proposes, applies, and the
patch is judged against what actually landed); and a read-only question
(survey, assess, answer — nothing written). The self-check-gated and
reproduce-gated shapes are not among the worked examples: both remain
reachable to a model plan only by naming their blocks individually through
the general grammar above, not through a pre-built template.

**Optional history.** `model_plan` and `attempt_gate_plan` both take
`history: Option<&str>`. When `Some` and non-blank, the prompt gains one more
section naming past plan shapes' use and resolution counts (see "Plan
history" below); when `None`, the prompt says nothing about history at all —
no heading, no blank section. `host::select_and_run` supplies this from
`cosmos::render_plan_history()`.

## The revision loop

`planner::attempt_gate_plan` drives one turn's whole attempt sequence: build
a plan (scripted or model, per `GatePlannerMode`), try
`polaris_cosmos::composition::materialize` against the sealed gate-block
catalogue and the host's own `CompositionLimits`, and on refusal submit a
whole new plan — never a patch to the refused one — up to
`MAX_GATE_PLAN_ATTEMPTS` (three: the first plan plus at most two revisions).
It returns every attempt made, oldest first, as plain
`Vec<planner::GatePlanAttemptResult>` — no evidence-store sequence number
attached to any of it, since only the caller, reading its own store at append
time, can assign one. Each element is either:

- `Materialized { choice, record, attempt, graph, native_signature,
  shape_label }` — a plan that materialised into a validated native graph,
  ready for the caller's own live admission and execution. `record` is the
  durable digest identity (`GatePlanRecord`: plan digest, catalogue digest,
  configuration-independent shape digest); `shape_label` is
  `planner::render_gate_expression_shape` applied to that same plan's own
  `GateExpr` tree while it is still in scope — a human-readable rendering the
  caller keeps instead of the tree itself (see "Visibility" below); or
- `Refused { choice, record, attempt, stage, refusals }` — the attempt was
  refused before a graph could run, either by `materialize` against a real
  `GatePlan` (`record` is that plan's own `GatePlanRecord`) or by the model
  grammar itself before a plan ever existed to materialise (`record` is
  `planner::invalid_model_answer_record`'s reserved invalid-submission
  identity — a real `GatePlanRecord` whose digest identifies the billed
  submission rather than claiming a `GatePlan` was built, never an `Option`,
  since Cosmos requires a gate-plan identity on every revision).

All but the last result are always `Refused`; the loop returns immediately
once an attempt materialises. The caller (`host::select_and_run`) turns every
attempt into a `CompositionDecision`/`CompositionValidation` pair and appends
them to the real evidence store in order — it is the only thing that knows
the store's own next position, so no sequence number is minted here.

A gate plan carries no fixed recipe name of its own, but the
`CompositionDecision` Cosmos records still carries an ordinary
`CompositionPlan` envelope, so a gate-plan composition is reported and
audited exactly like a finite-recipe one: a synthetic recipe id
(`polaris-code.recipe.gate-plan`) marks it as gate-plan-sourced, and the real
identity — the plan's own digest and configuration-independent shape — rides
alongside it as a `GatePlanRecord`, attached to the decision, and copied
automatically onto its validation, run and outcome by Cosmos's own
constructors.

## Honest completion

A model that changed nothing must not be reported as having resolved the
task. `CodingWorkspace::judge_verified` (`cosmos::payload`) now emits
`Status::Issue`, not a terminal pass, when an edit-verification graph
receives no stated edits at all — a statement-only task whose planner chose a
shape that ended up applying nothing has not established the task, so the
graph has not proven anything worth calling resolved. The narrower,
already-honest meaning of `NotApplicable` is preserved for the one thing it
actually measured: the separate `applied_edit_observation` still reports
`NotApplicable` when the caller named no exact edit, since that is a true
statement about *that observation*, not the task's outcome. A clean marker
scan (`cosmos::graphs`'s coverage audit finding zero remaining markers) is the
opposite case and is scored `Pass`: a real scan that establishes nothing
needs fixing is positive evidence, not an inapplicable check. As a backstop
for a future graph that skips the coding-specific helpers above,
`host::TopologyProduct::of`/`answer()` independently refuse to render a
product where every observation is `NotApplicable` as resolved.

## Plan history

The eval harness (`evals/core/plan_history.py`) can append one JSONL record —
`task_id`, `shape_digest`, the readable `shape`, `composition_policy`, gate
`planner`, the official grader `outcome`, and a `resolved` boolean derived
only from the grader's own disposition, never the agent's own terminal claim
— for every graded episode, when a pack's `run` command is given
`--plan-history PATH` (default `POLARIS_CODE_PLAN_HISTORY`; unset, history
stays off and nothing is written or read). `cosmos::plan_history::render_plan_history()`
is the read side: it reads that same env var, tolerates an absent, unreadable,
empty, or partly malformed file, groups rows by `shape_digest`, sorts by use
count, and renders at most ten lines such as
`Loop(until=tests-passed, run-project-tests-captured): used 12, resolved 3`.
`host::select_and_run` calls it once per gate-plan attempt sequence and passes
the result straight into `attempt_gate_plan`'s `history` parameter (see "The
model planner" above) — a best-effort prompt hint, never a requirement: a
missing or empty history changes nothing about whether a plan can be
submitted or admitted.

## Host wiring

`host::select_and_run`'s per-candidate loop tries the gate-plan path first
when the composition policy is `Gates`: a materialised plan is readmitted
through the same one-use native live-admission boundary
(`polaris_cosmos::topology::readmit_materialized_graph`) a catalog
registration's own recipe is, using the same real candidate the topology
selection decided among — a gate plan is not itself a catalog registration,
so the evidence store's own episode checks would otherwise refuse it — and
then runs through the unchanged execution path every other composition does.
An exhausted revision loop falls back to the finite recipe grammar for that
same candidate, never the next fallback candidate: nothing about the
candidate itself was refused, only the planner's own attempts, so retrying
the registration under the fixed recipe grammar is the honest next step
rather than moving on. A gate-plan attempt is never itself recorded as having
satisfied the coding contract unless one of its plans actually materialised
and ran; falling back and succeeding under the finite recipe grammar reports
as an ordinary finite-recipe composition — `ExecutedTopology::gate_plan_trail`
stays `None`, since it names only an admitted plan. The exhausted loop's own
refused attempts are not lost, though: they are retained separately on
`ExecutedTopology::exhausted_gate_plan_trail`, reach the headless JSON report
as `exhausted_gate_plan` alongside (never together with) `gate_plan`, and are
named in the interactive panel message, so a person can still see that a
candidate's gate planner tried and failed before the finite recipe grammar
took over.

Composition evidence — gate-plan or finite-recipe — is a plain value carried
on the executed answer (`ExecutedTopology::composition`,
`ExecutedTopology::gate_plan_trail`), never appended to the topology evidence
store as its own event: only the ordinary admission, run, and outcome events
are. A refused revision attempt has no real event to anchor to either —
materialisation refuses before a graph exists, so there is nothing to submit
to live admission. Nothing outside a gate-plan turn's own composition
evidence ever reads one of its `RecordedAt` values back against the real
store — the only real-store rule that reaches in is `CompositionRun`'s own
requirement that it sort after the validation it closes, checked against
whatever `RecordedAt` `execute_permitted` is handed for that run, not
against any particular admission position. So the whole trail for a
gate-plan turn — every refused decision and validation, the admitted pair,
and the run and outcome that close it — is numbered from one small counter
kept entirely apart from the store, rather than mixing local numbers for the
refused attempts with the real admission position for the admitted one:
reusing the real position there would leave no room below it for whatever
local revisions came first, since a real turn's own admission can land as
early as position two regardless of how many attempts were refused first (a
refusal never advances the store, no matter how many of them happen first).
`execute_permitted` takes this local run/outcome pair as an override,
supplied by the gate-plan caller alone; the finite-recipe path, which has no
revision loop to make room for, is unaffected and keeps reusing its own real
admission position throughout.

## The model-propose block's own executor

`code.model-propose` (`gate_blocks::configurable::ModelProposeBlock`) is the
one gate block that itself calls a model mid-plan — a plan built for a
test-gated edit uses it to propose the repair patch a failing test run
called for, and a statement-only edit uses it to propose the whole patch.
What that call actually does is a separate axis from the plan that wires the
block in: `gate_blocks::configurable::ModelStepExecutor` is the trait the
block's system reads through a `ModelStepProvider` resource, and which
implementation is installed decides whether the call is real.

`ScriptedModelStep` always returns one fixed `ModelStepArtifact`, regardless
of its `ModelStepConfig` or the workspace — the same role a scripted planner
plays for plan proposal, and what the test suite installs so a `gates` turn
is provable without a live model call. `LiveModelStep` wraps a real `Llm`
handle and runs one bounded, tool-restricted, budgeted turn:

- the model sees the step's `role` and `instruction` plus a fixed line of
  output directions naming which tool ends the turn and the exact JSON shape
  its arguments must have for the step's configured `ModelStepOutputKind`
  (`plan`, `findings`, `patch-intent`, `review`, or `answer`). For
  `patch-intent`, those directions also require, before submitting: naming
  the exact code path (file and function) that produces the reported
  behavior, and one scoped `search` for analogous variants — other subclasses,
  managers, backends, or overloads named like the one being changed — that
  may need the same edit. This is a prompt-only nudge, not a check the
  executor enforces; it targets the two failure modes where a patch
  landed on a plausible but wrong branch, or where an analogous implementation
  sharing the changed code's name was left unexamined;
- the step's turn always offers one `submit_answer` tool — independent of
  `ToolSubset`, since even a step with no exploration tools at all must be
  able to answer — whose input schema is generated from the configured
  output kind (`submit_answer_tool_definition`), `kind` itself pinned to a
  one-value enum so a schema-conformant call can never name the wrong one.
  Calling it with well-formed arguments ends the step immediately: the turn
  loop decodes them with the same `ModelStepAnswer`/`decode_model_step_answer_value`
  used everywhere else, so the tool-call path and the legacy plain-JSON path
  share one decoder, not two. Malformed arguments (wrong `kind`, missing or
  mistyped fields) come back as a tool error naming what was wrong, which
  costs the step a turn rather than ending it — the model sees the reason and
  can call `submit_answer` again. A `patch-intent` answer is additionally
  checked edit-by-edit before it is accepted, win or lose: each edit must
  resolve against the real workspace via `edit::verify_exact_edit`, the same
  exact-match decision `apply_patch_intent` itself uses later, so an edit
  whose `find` text is missing or ambiguous is refused as a tool error naming
  which edit and why, before it can ever reach the apply step;
- a plain-text reply (no tool call at all) is still accepted, but only as a
  lenient fallback: the reply is stripped of a ` ```json `/` ``` ` fence and
  any surrounding prose (`extract_json_object`), and the remainder must still
  decode as a valid, `kind`-matched answer for the configured output —
  including the same per-edit check for `patch-intent`. Anything that fails
  that — free prose, an empty reply, a `kind` mismatch, a malformed field —
  gets a plain-text reminder to call `submit_answer` instead, costing a turn
  rather than ending the step on the first bad reply the way the block's
  original single-parse text contract did;
- once a step is down to its last three turns, the next outgoing turn (a
  tool-result turn if the model just made tool calls, or a reminder turn
  otherwise) carries an explicit notice naming how many turns remain and that
  the model must submit now (`turns_left_notice`) — `max_turns` itself is
  unchanged by this; the notice only makes the existing cap visible in-band
  before it is reached rather than only after. A second, lighter notice fires
  once, at the turn halfway through the step's own `max_turns`
  (`exploration_checkpoint_notice`): a nudge to name the concrete failure path
  found so far, or submit now if that is already enough, aimed at a long
  exploration that has not converged rather than one already near its cap.
  `turn_notice` picks between the two — the closing notice wins whenever both
  would apply, so a step never receives more than one notice on the same
  turn, and neither notice changes `max_turns` itself. Both notices count from
  this one call's own turn 0: a resumed (repair) round
  (`code.resume-model-propose`) gets its own fresh `0..max_turns` budget, so
  each round gets its own closing warning and its own midpoint nudge rather
  than inheriting a countdown from turns already spent in the round it
  resumes. A step that reaches its last turn with nothing but tool calls or a
  bad reply gets a typed fallback `Answer` naming that it did not submit,
  rather than an error, so a plan's downstream judge still has something to
  read;
- it may call `read_file`, `list_dir`, `search`, and/or `run_python` —
  independently, only the ones the step's `ToolSubset` names — no other tool
  call is ever permitted, and naming any other string in the subset grants
  nothing, so a read-only role stays read-only regardless of what a plan's
  configuration claims. `list_dir` lists one sandbox-relative directory's
  immediate entries,
  bounded at a fixed entry cap. `read_file` takes an optional `start_line`/
  `max_lines` range, sharing `tools::code::read_view`'s line-numbered
  rendering with the interactive `read_file` tool, so a range short of a
  file's end states the total line count and the exact `read_file(...)` call
  that continues it, rather than only a full-file read capped at 32 KB with no
  way to reach anything past it. An exact duplicate `read_file` call — same
  path, same range — inside one step gets a short note instead of the same
  text again, since a read-only step's own workspace cannot change underneath
  it between two of its own turns.
  `search` looks for a literal string or a regular expression, optionally
  scoped to a `path` (a file or directory, rather than always the whole
  sandbox tree), with `context_lines` (0–5) of surrounding lines per match and
  a `files_only` mode that lists just the matching paths. It is bounded by
  files walked, bytes per file, matches returned, and the rendered result's
  own byte cap — the same shape of bound the marker-scan gate blocks already
  use, over an arbitrary caller-chosen pattern instead of one plan-fixed
  marker — and walks with the same `.gitignore`-aware, `.git`-skipping
  selection the interactive `search`/`glob` tools share, rather than a
  hand-rolled walk with its own, narrower idea of what to skip. Whenever the
  file walk or the match count stops at its own cap, the result says so
  explicitly (which cap, and to narrow `path`) instead of a bare "no
  matches", which a model cannot tell apart from a real absence. A
  `context_lines` above the cap is clamped to it rather than refused, because
  the tool's own JSON schema already establishes that bound, and the clamped
  result says so.
  A `literal=true` search that matches nothing and whose pattern contains
  regular-expression syntax (such as `|`) gets an added note that literal
  mode matched those characters exactly rather than interpreting them — most
  surprisingly, `|` is not OR — so an empty result is not misread as a real
  absence when the pattern was written as if it were a regular expression.
  `run_python`, only offered when the plan sets `offer_check` (the
  self-check-gated shape's own `propose-0`/`propose-1` configs), writes a
  script the model wrote to a scratch path under `.polaris/` and runs it
  through the same sandboxed `run_python_script` `code.run-check-script`
  itself uses later — timeout and output-cap identical — so the model can see
  a script's real output before deciding what, if anything, to submit as its
  answer's `check` field;
- every turn's usage — full-price input and output plus any cache-read and
  cache-creation tokens the provider reports — is billed onto the step's
  `TokenMeter` before the budget check runs, so an over-budget turn is a
  refusal over an already-billed, auditable call rather than a discarded one.
  `TokenBudget` bounds *that one turn's own context* (its input, cache tiers,
  and output together), never the sum across every turn the step takes — a
  step that takes many turns, each comfortably under budget, keeps going;
  `max_turns` is what bounds total work, the same way a session's own spend
  ceiling bounds total cost rather than any one call;
- each request caches the stable tool-definition prefix and places cache
  breakpoints at the turn's own start and its latest message — the same
  breakpoint choice `agents::context::CacheAwareWindow` makes for the main
  loop's window, reused here — and, once the growing turn history passes a
  compaction threshold sized for a step's own shorter, bounded life
  (`MODEL_STEP_COMPACT_AFTER_TOKENS`, later than the main loop's own
  `DEFAULT_COMPACT_AFTER_TOKENS`, since stubbing rewrites the whole cached
  prefix at the cache-write price and a step capped at a few dozen turns
  rarely reads enough later turns to earn that back), the oldest tool
  results are stubbed in place exactly as `CacheAwareWindow` stubs them. Both
  keep a long exploration's resent prefix cheap and cache-stable rather than
  rebilled at full price every turn;
- the turn is capped at the step's own configured `max_turns` (bounded at
  decode time to a small ceiling) — enough for however many tool calls the
  role needs plus the follow-up answer, but still bounded because a model
  step is one action inside a larger plan, not an open-ended agent loop. A
  configuration built through `ModelStepConfig::new` and never given
  `with_max_turns` keeps the four-turn default this block always used before
  the cap became configurable;
- a step configured for `patch-intent` with
  `ModelStepConfig::sees_prior_failure` set also reads an upstream
  `TestRunOutcome` and folds its captured failure into the instruction before
  calling the provider, so a repair retry sees *why* the previous attempt did
  not pass rather than being told only, statically, to try again. The
  materializer refuses a plan naming this flag with nothing producing a
  `TestRunOutcome` in scope, before execution rather than at run time;
- with `ModelStepConfig::review_before_submit` on (off by default; on for
  every `propose-edit`/`propose-edit-repair` config
  `statement_edit_model_step_config` builds — the statement-only-edit plan's
  own propose/repair step, never a judge, since `judge_patch_intent` is a
  deterministic check rather than a model step at all), the step's first
  accepted `submit_answer` call is answered with a one-time review notice
  instead of ending the step: name the failing operation from the task
  statement, name the code path that produces and consumes the changed value
  plus every named output/variant/mode/downstream consumer the statement
  mentions, say which of those the patch covers and read and extend it for
  any it does not, confirm the patch removes or disables no behavior the
  task did not ask to change (targets a patch that silently swapped in an
  `if False:` over a live, unrelated branch), confirm that a producer/
  consumer pair — a writer and its reader, or a serializer and its parser —
  still agree if the patch touched only one side, and (if `run_python` was
  offered) run one check showing the full statement behavior after the
  patch — then submit again, which is accepted. This costs one extra turn,
  so the flag also adds
  `REVIEW_TURN_ALLOWANCE` (4) turns on top of the step's own `max_turns`
  before any of the turn-notice or last-turn-forces-`submit_answer` logic
  above is applied, so a step that spends its one review still has turns left
  to act on it; if the first accepted `submit_answer` call already lands on
  that extended true last turn, there is no turn left to spend on a review
  and it is accepted directly instead. Only one review ever fires per step,
  and on the turn it fires the ordinary turns-left/midpoint notice is
  suppressed rather than stacked alongside it, since the two would otherwise
  give contradictory instructions ("submit now" next to "keep working, then
  resubmit");
- the provider call and every tool call each race the run's own cancellation
  signal, so a cancelled run stops the step at its next await instead of
  finishing an in-flight model call or file read first;
- whichever path accepts the final answer — a `submit_answer` call or the
  lenient text fallback — the underlying decode is still strict against the
  configured output kind: a `kind` that does not match the configuration or a
  `kind`-matched shape with malformed fields are distinct typed refusals
  (`SystemError`, never a panic) from `decode_model_step_answer`/
  `decode_model_step_answer_value`, not silently coerced into an artifact —
  what changed is only that a bad answer no longer ends the step outright,
  since both acceptance paths now give the model a turn to correct it first.

## Host wiring for the live executor

Each gate-plan execution also installs a fresh `ModelStepHandoff` local
resource. A model step records its submitted artifact, touched paths, tool
calls and results, stated open questions, and whether old tool output was
compacted. The record is separate from its opaque `ModelConversation`.
`ModelStepConfig::with_context_mode` selects what the next step reads:
`fresh` starts from its own instruction; `brief` adds a structured summary
of at most four prior steps and 4,096 characters; `continue` resumes the
last model conversation with its tool results. A continued step with a
different output kind receives that kind's answer directions. The live
executor applies the existing `compact_tool_results` routine before each
request, at the smaller of its usual 120,000 token threshold and the new
step's per-turn budget. The handoff records whether the resulting conversation
contains stubbed results. Existing plans omit the new config field and retain the same
encoded configuration and fresh behavior. The runtime model planner can
name `context_mode` in each model-propose step's JSON configuration.

`host::select_and_run` builds each attempt's live context on the session's
own globals, so a graph can already reach the shared `ModelRegistry` the
gate planner itself reads. Right after that context is built, the host
resolves an `Llm` handle from the same registry entry and model id the
planner uses; when both resolve, it installs
`ModelStepProvider::new(LiveModelStep::new(llm).with_cancellation(cancel))`
into the context, `cancel` being the same run cancellation signal a system
inside the graph reads, so any plan that wires in `code.model-propose` calls
a real model that stops promptly if the run is cancelled. When either is
absent — no configured provider, or no model id for this run — nothing is
installed, and a plan that reaches for the provider fails live resource
validation with a plain missing-resource refusal rather than a panic. Tests
that need `code.model-propose` install `ScriptedModelStep` directly instead
of going through this resolution.

The same construction also installs `LiveModelStep::with_shell(...)` whenever
a `ShellExecutor` global resource is present in the run's context, so a live
`run_python` tool call and `code.run-check-script` both shell out under the
same session shell policy every other tool already honours — never a separate
policy carved out for this one feature. Absent, `run_python` fails closed (see
`run_python_script`'s own `Option<&ShellExecutor>` parameter), matching
`code.run-check-script`'s own behaviour when its required `ShellExecutor`
resource is simply missing from the graph's context.

## Env knobs

`POLARIS_CODE_COMPOSITION_POLICY` now accepts `gates` alongside
`deterministic` and `model`. `POLARIS_CODE_GATE_PLANNER` selects which
planner a `gates` turn uses — `scripted` (default) or `model` — and is
validated at startup the same way, whether or not the composition policy is
actually `gates`. Both can instead be installed as global resources
(`composition::CompositionPolicy`, `planner::GatePlannerMode`) before the
server starts, which is how the test suite exercises them. A third resource,
`planner::GatePlanLimits`, overrides the materialisation bounds a `gates` turn
checks a plan against; unset, a turn uses
`CompositionLimits::conservative()`. `POLARIS_CODE_PLAN_HISTORY` (see "Plan
history" above) names the JSONL file a model planner's prompt draws past plan
shapes from; unset, no history is read or written and the prompt says nothing
about it.

## Visibility

An executed gate-plan turn's panel message names the admitted plan's own
digest and shape digest, its position in the revision loop, and how many
earlier attempts were refused first (`terminal::topology`'s
`gate_plan_summary`, alongside the existing "why this one"/"alternatives
considered" selection explanation). The headless JSON report carries the same
facts, structured, under `topology.gate_plan` (`exec::report::GatePlanReport`)
— present only when the executed composition's own revision trail is.
`GatePlanReport::shape` additionally carries `planner::render_gate_expression_shape`'s
rendering of the whole admitted gate tree — every gate's own name, nested
exactly as the plan nests it, down to the block/predicate/discriminator names
at its leaves, e.g.
`Seq(model-propose, apply-patch-intent, Loop(until=tests-passed, run-project-tests-captured))`
— computed once, in `planner::attempt_gate_plan`, from the plan's own
`GateExpr` tree while it is still in scope, and carried from there as plain
data (`GatePlanAttemptResult::Materialized::shape_label`) through
`host::select_and_run` to the report. This is a full-tree rendering, not a
flat, unordered block list: two plans built from the same blocks in a
different shape — a `Seq` versus a `Choose` between them, a fixed count
versus an until-predicate `Loop` — render differently, which a flat list
cannot show and which matters once "Plan history" groups turns by shape.
`shape_digest` (`GatePlanReport::shape_digest`, read straight off
`GatePlanRecord::shape`) remains the canonical, configuration-independent
identity two admitted plans are actually grouped or compared by; `shape` only
makes that identity legible to a reader, and nothing compares two `shape`
strings for equality anywhere the digest already is. `GatePlanReport::planner`
names which planner assembled the plan —
`polaris-code.gate-planner-scripted` or `polaris-code.gate-planner-model` —
read off the admitted decision's own autonomous choice origin, the same
policy identity `GatePlannerMode::as_str` names.

## Domain plans and arms

`cosmos::domains` (`crates/polaris_code/src/cosmos/domains/`) is a second,
orthogonal axis over the scripted planner. It asks whether a plan informed by
one project's conventions performs differently from the generic plan.

A run selects a **domain** (`none`, `django`, `sympy`, `sphinx`) and an **arm** (`generic`,
`generic-notes`, `expert`, `experience`) — `cosmos::domains::Domain`/`Arm`, combined into one
`DomainSelection`. `POLARIS_CODE_DOMAIN`/`POLARIS_CODE_ARM` set them, validated together at
startup the same way the two knobs above are: an unknown domain, an unknown arm, or an arm other
than `generic` paired with `none` all refuse before the server starts, naming what is accepted.
Unset, both default to `none`/`generic` — the plan below, unmodified.

`domains::plan_for_selection` is where `attempt_gate_plan` calls this in place of a plain
`scripted_plan` call whenever the gate planner mode is `Scripted` (the model planner is
unaffected — it always chooses freely). The four arms:

- **generic** calls `scripted_plan` completely unchanged — the same function, same
  arguments, same output — regardless of the selected domain. A test
  (`domains::tests::the_generic_arm_reproduces_scripted_plan_for_every_domain`) checks every
  domain's plan digest against `scripted_plan`'s own, not just that the code path looks
  unmodified.
- **generic-notes** keeps the same plan shape and calls
  `planner::scripted_plan_generic` — `scripted_plan`'s own body, generalised to take the
  statement-only branch as a parameter — with the selected domain's **expert** notes appended to
  the statement-only edit shape's model-step instructions (see below).
- **expert** and **experience** call the selected domain's own
  `expert::plan`/`experience::plan`, which supplies the statement-only branch itself: everything
  else (the read-only question shape, the stated-edit shapes) is unchanged, since a domain
  variant's whole contribution is a plan and notes for the statement-only edit case a SWE-bench
  issue actually is.

A domain has two variants, `expert` (the expert arm's researched notes and plan; the generic-notes arm borrows its notes)
and `experience` (the experience arm's own notes and plan, derived from prior generic-plan
transcripts rather than external guidance). Every variant starts identical to the generic plan — empty notes, no
extra gate blocks, and a plan that is `planner::statement_only_plan_body(facts, attempt, NOTES)`
with its own (currently empty) `NOTES`. A second test (`every_domain_variant_builds`) checks
that every variant builds a plan; only the generic arm is pinned to the generic digest. `cosmos/domains/README.md`
is the "how to add a domain plan" note: which files a domain
owns, how to add a block, what a plan function receives, and how to test a change.

`planner::statement_only_plan_body(facts, attempt, notes)` is the one place `notes` reaches a
model step's instruction (`statement_edit_model_step_config`'s own fourth parameter) — the generic-notes arm calls
it directly with the domain's `expert::NOTES`, and a domain's own `expert::plan` calls the same
function with the same constant, so generic-notes and expert always carry byte-identical notes text for as long as
neither reformats it before passing it in. Domain blocks are wired into the sealed coding block
catalogue unconditionally (`gate_blocks::coding_block_catalogue` calls `domains::register_all`,
every variant's own `register_all` a no-op until a block is added), so a new block
needs no edit outside its own variant file.

`DomainSelection` is a session global exactly like `GatePlannerMode`, read once per turn
(`host::read_domain_selection`) and installed once at startup from the validated env vars
(`main.rs`); absent, it defaults to `{domain: None, arm: Generic}`, which is why an unconfigured
run's gate-plan turns are unaffected by this module's existence at all.

## Lean arm

`--arm lean` (overrides `POLARIS_CODE_ARM=lean`) selects the domain-independent
lean statement-only plan under the scripted gates policy, just as `loop` selects
the loop arm. Lean selects that policy automatically when no composition setting is
given or when the harness sends its deterministic baseline; model composition
or a model gate planner is refused at startup. The harness also accepts
`--arm lean`; both require domain `none`.
`planner::lean::lean_plan_body(facts, attempt, &LeanOptions)` is the one extension
point for later lean systems. The loop arm's plan and encoded defaults are unchanged;
`the_loop_arm_differs_from_generic_and_materialises` pins its pre-lean digest.
Lean uses an extended sealed block catalogue for its cascade blocks and
predicate. Other arms keep the original catalogue digest and plan digests.

Lean has one directly editing model conversation. The optional cheap locate step
precedes it and hands over only its bounded findings; `POLARIS_CODE_LEAN_LOCATE`
defaults to `1`, while `0` removes it. If the host's shell-availability probe succeeds, the editing step offers
`run_python` and `run_command`; the latter uses the installed `ShellExecutor` and
its unattended command permissions. Commands run in the workspace with a 300-second
timeout. A bounded live sink retains each pipe's head and tail, including the tail
beyond the executor's prefix capture cap; the response is below 10,000 characters
and reports the exit code and timeout. A one-sentence prompt tells the model it
can run the project's own tests. The checkout-only/no-upstream-fetch rule remains.

For the Python harness, pass exactly `--allow-command "python *"` and
`--allow-command "pytest *"` (alongside `--allow-edit`). These permit
`python -m pytest ...`, `python script.py ...`, and `pytest -q ...`; no shell
wrapper grant is needed. The host availability probe requires the Python grant.
Bare `pytest`, `python3 ...`, and other executables need separate explicit grants;
use the matching command forms with this policy. The command adapter never widens
the installed grants.

The cap is 100 model request attempts, or an explicitly configured
`POLARIS_CODE_MAX_TURNS` if lower. With `POLARIS_CODE_LEAN_CASCADE=1`, that cap
covers up to three locate turns, 18 editing turns in tier 1, and the unused
remainder in tier 2 only if a system check fails. The check reads the turn meter,
so an early tier-1 answer leaves its unused turns to tier 2. A configured lower
ceiling applies to the whole plan; values too small for two editing turns are
rejected. Even malformed last-turn submissions stay within the configured step
cap. The per-request token budget is `u32::MAX` (a nonbinding sentinel); the
provider's own context and configured output limits still apply; an active
cost cap additionally bounds each model-step response to 8,192 output tokens.
Lean batches
old tool-output trimming by default, including in the warm continued
conversation. `POLARIS_CODE_LEAN_TRIM=0` disables it. Existing arms retain
their automatic compaction and last-submission repair behavior.

`LeanOptions::from_env` reads `POLARIS_CODE_LEAN_TRIM`,
`POLARIS_CODE_LEAN_AUTOTEST`, `POLARIS_CODE_LEAN_LOCATE`, and
`POLARIS_CODE_LEAN_CASCADE` as `0`/`1`. Locate, trim, and cascade default on;
autotest also defaults on. It also reads the optional
`POLARIS_CODE_LEAN_LOCATE_MODEL` and `POLARIS_CODE_MAX_COST_USD` (finite nonnegative
USD). Lean defaults per-task estimated cost admission to $1.50; `0` disables
it, and other arms default off. The task meter covers Haiku locate and both
Sonnet tiers. Admission estimates input as serialized request bytes / 4, plus
25% and 512 tokens for token density and provider framing. It reserves the full
allowed output. Cold input reserves cache-write pricing. For the same model, a
byte-identical prefix confirmed by the previous response's cache usage earns
cache-read pricing for 90% of those confirmed tokens. New text and the remaining
allowance reserve cache-write pricing. Changed tools, system text, tool choice,
output schema or prefix messages remove that discount; it also expires four
minutes after the earlier request started. Calls without confirmed cache usage
get no discount. Provider usage replaces each reservation when the call ends;
spend at or over the cap prevents the next call. Missing usage spends the
reservation; a cancelled call releases it.

This is an estimate, not a hard billing guarantee: unusual token density or a
provider cache miss can exceed it. Debug tracing records reserved and settled
costs for comparison. Other arms can opt into an explicit cost limit. Only lean
reads the other lean switches; TWM switches also apply to `loop`.

`POLARIS_CODE_LEAN_CHECK_LOCATE=1` asks the editing step to read the lines
named by the location report and confirm the symptom there before editing;
`0` restores the old wording. `POLARIS_CODE_LEAN_STABLE_TOOL_CHOICE=1` keeps
the last request's tool choice unchanged, including on warm continuation;
`0` restores the forced final `submit_answer`. Both default on in lean only.
The strict turn cap still applies if the model does not submit. Warm
continuation shortens executed autotest results, but leaves an "Autotest not
run" message intact. Trim payback counts only possible requests after the
request that writes the changed prefix. Haiku 4.5 has a 4,096 token minimum
cacheable prefix, so the short location requests can remain uncached even
though they already carry cache breakpoints.
These defaults are set in the binary. The eval harness forwards
`POLARIS_CODE_ARM`, its deterministic baseline, and any host variable starting
with `POLARIS_CODE_LEAN_` or `POLARIS_CODE_EXP_`, so a run can override them.

Cascade requires the policy-allowed Python shell. At the start of the plan it
records the initial Git changes and seeds `.polaris/lean-repro.py` with an editable
placeholder. The model replaces that placeholder with an assertion. After tier 1,
the plain-code check requires a change relative to that initial snapshot, runs
the assertion against a disposable copy restored to the initial source bytes,
requires an `AssertionError`, runs it against a separate disposable patched copy
and requires exit zero, then runs up to three existing nearby `test_<module>.py` files with
`python -m pytest`. Those test files are restored to their pre-tier-1 bytes in
the patched copy, so editing a test cannot weaken this check. Any missing
script, denied command, missing matching tests,
timeout, or setup failure continues to tier 2. Both disposable copies live under
`.polaris` and are removed after the check; the real source tree is never rolled
back or used to run checks. The predicate `code.cascade-passed` uses the typed check result in a
`Choose`: pass concludes with an indeterminate coding verdict, while failure
resumes the same captured model conversation in `ContextMode::Continue` with a
short failure reason. The indeterminate verdict avoids claiming full task
correctness from a focused system check. When automatic tests already passed
under pytest for every nearby test, the cascade reuses that result if the
workspace's changed-file contents and the original test files are still
identical. Otherwise it runs its own nearby tests in the disposable copy.

Headless `usage.num_turns` counts model request attempts, including failed calls,
from the session meter; the harness copies it to episode `num_turns` and retains
it in `usage`. For the harness's fresh session per task, this is the task's count.
It is distinct from `turn_number`, which counts user turns. Older records remain
unknown (`null` at the episode level), never inferred or reported as zero.

### Lean automatic tests and navigation

`POLARIS_CODE_LEAN_AUTOTEST=1` enables the routine-work system only in lean;
it is the lean default, and `0` disables it. Successful writes in one model reply are batched:
exact edits supply their paths, and `run_command` / `run_python` use bounded
before/after source-file metadata to detect changed or created files.
Generated bytecode/cache files do not trigger another test run. Lean has no
separate file-creation tool. One test command runs after the batch and appends
at most 900 characters to the last successful write result, before the next
model request. Failed or already-satisfied
exact edits do not trigger tests. No extra model request is made.

Selection uses the checkout's `tests/runtests.py` or `runtests.py` (dotted test
labels), then `bin/test` (file arguments), then pytest when `pytest.ini`,
`conftest.py`, or pytest configuration in `pyproject.toml`/`setup.cfg` identifies
it. Edited Python test files (including `tests.py`) select themselves. Otherwise `test_<stem>.py` /
`<stem>_test.py` matches take priority, followed by tests sharing a package
directory. Path overlap ranks ambiguous matches. At most eight files from a
20,000-file, ignore-aware walk are selected; no whole-suite fallback occurs.
Unknown runners or mappings skip quietly. These are conventions, not proof of
coverage: custom runners may need flags or accept different label grammars.

Commands use the installed `ShellExecutor` and its existing grants, with a
120-second process timeout, cancellation, bounded pipe draining, and a 125-second
outer deadline. Selection and shell-change snapshots each have a two-second
wait bound. Python runners use the existing `python *` grant; `bin/test *`
needs its own grant. Refused commands say they were not run. Streaming capture
retains counts and the first failure even between long logs; unrecognised
summaries report unavailable counts. A subset pass never means task success.
Snapshot detection may miss ignored/oversized files or edits preserving both
size and timestamp; direct `edit_file` paths do not depend on snapshots.

The same switch adds `find_definition` and `find_references` to lean's requested
tools. The live executor offers them only if the existing code index is ready
and its canonical root matches the task's workspace. It reuses
`CodeGraphTools` readiness/freshness checks, caps returned text below 1,500
characters, and bounds the async query wait to two seconds. There is no second
index build. Absent, cold, failed, or differently rooted indexes omit these tools;
`search` remains available. References are syntactic callers, not complete
semantic usage or dynamic dispatch. Negative queries may reconcile the tree;
a timed-out background reconciliation may still finish after the caller falls
back to search.

Automatic-test feedback is capped at 900 characters. Batched trimming can
replace old edit and test feedback with a short stub; warm continuation also
shortens prior automatic-test results at its handoff boundary.

Executor hook: `ModelStepConfig::autotest` defaults false and is omitted from
encoded configurations when false. `lean_plan_body` alone enables it from
`LeanOptions`; other arms retain their plan digests. Batched trimming also handles old edit results carrying autotest feedback.
Autotest feedback is observational and
does not gate submission or change the existing unverified conclusion.

### Typed working memory

`POLARIS_CODE_LEAN_TWM` (default off) applies to the `lean` editing step and
plain `loop` arm. It replaces the step's
growing transcript with a small set of typed slots
(`cosmos::gate_blocks::twm::TwmState`): a task brief, a working set of files
read (with their current content), the current diff, the last test result per
command, the last error per tool, and a turn budget. Plain rules — never a
model call — keep each slot current as the step's tool calls and results come
in (`TwmState::observe`). Between rebuilds the conversation stays append-only.
A rebuild happens only when the whole next request exceeds
`POLARIS_CODE_LEAN_TWM_REBUILD_TOKENS` (default 40,000). The estimate is the
serialized request's character count / 4: system text, tool schemas, every
message, the prior rendered slots and kept tail, plus JSON framing. It is not
growth since the last rebuild. Short tasks never rebuild just because turns
pass.

The rebuild replaces history with the typed slots plus the last
`REBUILD_KEEP_TURNS` (2) assistant turns and their tool results, kept together.
Once that smaller prompt is below the limit, it grows by appending until it
crosses the same whole-prompt limit again. An unusually large kept tail or a
limit smaller than the rendered slots can still leave it above the limit;
this is a size trigger, not a hard prompt cap. TWM replaces both the jump
compactor and batched trim for that step; `config.trim` and
`config.compact_history` are ignored while `ModelStepConfig::twm` is set.
The slot schema and rebuild policy are implemented in
`cosmos::gate_blocks::twm`.

Three switches extend it, each independent and each off by default:
`POLARIS_CODE_LEAN_TWM_NOTE` offers the model one small (500-character) free
text `note` tool — the only slot the model itself writes, the rest are all
rule-derived; `POLARIS_CODE_LEAN_TWM_TESTSTATUS` captures the existing
autotest summary text as a typed `TestStatus` entry instead of leaving it
only spliced into a tool result; `POLARIS_CODE_LEAN_TWM_CODEMAP` builds a
free, no-model-call map of top-level definitions across a bounded file sample
(`twm::build_codemap`) once at step start, placed in the unchanging first
block. These three switches require the TWM master switch, and none enables
another. They also work on `loop` without enabling lean's locate, autotest,
trim or cascade systems. `ModelStepConfig::twm` is omitted from encoded
configuration when unset or off, preserving the original `loop` plan digest.
Other arms ignore all TWM switches and retain their plan digests.

Offline tests cover 30 short turns without a rebuild, a whole-prompt threshold
crossing followed by a short turn without another rebuild, and unchanged loop
digests when off. A scripted billing fixture (27,000 input and 1,000 output
tokens) reserves between 1x and 1.5x its cold bill and between 1x and 3x its
  cached bill. Tests also check changed or expired prefixes and refusal after
  real usage exceeds the cap. These are scripted checks, not live tokenizer
  results.
