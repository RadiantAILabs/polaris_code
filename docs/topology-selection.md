# Topology selection in the live host loop

`polaris-code` can serve a normal conversational turn or execute one bounded
coding topology. Both paths share the outer session driver in
`crates/polaris_code_core/src/run.rs`; topology selection occurs there before
the conversational agent graph starts.

An empty coding-task queue leaves the topology path inert. No catalog lookup,
selection, graph execution, or topology evidence is produced, and the ordinary
agent loop proceeds unchanged.

## Lifecycle

For an accepted coding task, `cosmos::host::select_and_run` performs these steps
in order:

1. profile the task into the closed Cosmos feature vocabulary;
2. read the sealed catalog and current host availability;
3. mechanically admit or exclude registrations;
4. apply any per-task directive or run-wide pin;
5. rank eligible candidates and record a `SelectionDecision`;
6. choose or assemble the selected registration's graph recipe;
7. readmit that concrete graph against live host state;
8. execute at most one graph;
9. run ordered verifiers and append outcome evidence;
10. project the recorded decision and evidence into the terminal result.

Admission and selection semantics come from `polaris_cosmos`; the coding host
owns task acceptance, context construction, live capability probes, execution,
and coding verifiers.

## Per-turn composition

Selection chooses a registration. Composition determines the concrete recipe
used for that registration on this task:

- `deterministic` selects from the registration's bounded recipes using task
  facts and declared costs;
- `model` asks a model to choose from that same bounded recipe grammar;
- `gates` assembles a task-specific graph from the coding gate-block catalog;
- `gates-model` uses the model gate planner while preserving the same bounded
  grammar and admission boundary.

The model cannot invent a registration or execute an unregistered block. A bad
or unavailable model choice is reported through the composition error path.
Gate-plan materialization and revision are described in
[`gate-planning.md`](gate-planning.md).

## Coding tasks

A `CodingTask` is a unit of work, not necessarily a precomputed edit. It holds
a statement, workspace, scope, intent, and `SuccessCheck`; it may additionally
carry exact stated edits.

The narrow `CodingTask::new` constructor remains useful for one exact
find-and-replace. `CodingTask::from_statement` represents work whose solution
must be discovered. `SuccessCheck` tells the accepting side how success will be
judged: exact edits, a caller command, or delegated verification.

Task intent (`fast`, `careful`, `thorough`, or `answer`) becomes task-profile
facts rather than a direct registration name. Read-only questions and bounded
survey tasks are represented explicitly so admission can rule out mutating or
underspecified shapes.

## Accepting work

Interactive work enters through `/topology`; unattended work uses
`--topology-task` with the same `key=value` specification. An exact edit can be
submitted as:

```text
file=src/lib.rs find="old" replace="new" intent=careful
```

A statement-only task can supply a statement, scope, optional verification
command, and intent. Unattended mutating work also requires the normal
`--allow-edit` grant; topology submission does not bypass tool permissions.

The task is parsed and validated before it enters `CodingTaskQueue`. Unknown
fields, malformed edits, paths outside the working directory, and unknown
topology directives are refused without queueing work.

## Per-task direction

The optional `topology=` field is a command for this task:

- `topology=<registration-id>` narrows the candidate set to exactly that
  registration;
- `topology=none` records that no topology should start;
- `topology=auto` leaves selection autonomous.

A named registration is validated against the sealed catalog at acceptance.
If it is later unavailable or inadmissible, the turn reports that
registration's own exclusion reasons and does not substitute another one.

`ExecutedTopology::requested` and `NoTopologyStarted::requested` distinguish a
directed result from an autonomous one in the turn report. The aggregate
Cosmos outcome record does not yet retain that provenance; see
[`known-issues.md`](known-issues.md#topology-host).

## Run-wide pin

`POLARIS_CODE_TOPOLOGY_PIN` pins tasks that do not carry their own directive.
The value is validated once at startup against the sealed catalog. A per-task
directive takes precedence because it is closer to the work being submitted.

The pin changes neither catalog contents nor admission. If the pinned
registration cannot run for a task, the result is a refusal with its recorded
grounds, not a fallback selection.

## Automatic routing

The host may translate a narrowly recognizable prompt into a coding task before
starting the conversational loop. Routing is conservative and preserves the
prompt as the statement. A prompt that does not meet a supported shape stays on
the normal conversational path.

`TaskOrigin` records whether a task was submitted explicitly or auto-routed.
That fact can adjust selection policy without changing task contents or
permission posture. Read-only questions remain non-mutating; edit-shaped tasks
must still pass admission and permissions.

## User-visible result

A topology result has four broad forms:

- one registration executed and produced ordered verification evidence;
- a directed request selected none;
- no candidate was eligible, with exclusion grounds;
- selection, composition, admission, execution, or verification failed.

An executed result includes the selection rationale and every alternative the
decision considered. Rendering reads the stored `SelectionDecision`; it never
re-ranks candidates. The interactive panel and headless JSON report share this
projection. See
[`topology-selection-explanation.md`](topology-selection-explanation.md).

## Identity and side effects

Topology event identities derive from the session and turn identity so replay
can reproduce evidence keys. The host executes no more than one registration
for an accepted task. The graph receives a pristine `SystemContext` populated
only with declared coding resources, middleware, and user I/O needed for that
run.

Evidence records live in the session store. `/topology compare` is different:
it uses disposable workspace copies and a single-use evidence store per arm so
comparison cannot alter the caller's workspace or later selection statistics.
See [`topology-comparison.md`](topology-comparison.md).

## Verification

The behavior is covered by:

- `tests/topology_selection_host_loop.rs`;
- `tests/topology_work_affordance.rs`;
- `tests/topology_directed_selection.rs`;
- `tests/topology_operator_pin.rs`;
- `tests/topology_ux_interaction.rs`;
- `tests/topology_effort_measurement.rs`;
- `tests/topology_effort_routing.rs`.

Run them through `cargo +1.97.0 test --workspace`.
