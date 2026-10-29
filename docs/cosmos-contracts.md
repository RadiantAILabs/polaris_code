# Cosmos topology-runtime contracts in `polaris-code`

`polaris_cosmos` defines domain-neutral contracts for task profiling,
topology admission, selection, execution evidence, and later outcome
observations. `polaris_code_core` provides the coding-specific types,
registrations, host capabilities, and verifiers that make those contracts
executable.

## Coding task boundary

A `CodingTask` contains:

- a problem statement;
- a workspace and declared scope;
- an optional stated edit or a statement-only task;
- a `SuccessCheck`, supplied as stated edits, a caller command, or delegated
  verification.

The profiler maps those facts into Cosmos's closed feature vocabulary. It does
not inspect a proposed solution or infer capabilities that the host did not
report.

## Catalog and type explanations

Coding topologies are registered in
`crates/polaris_code_core/src/cosmos/topology.rs`. Each registration describes
its purpose, graph recipe, required resources, graph-boundary types, and author
priors.

Every boundary type must be explained by the type registry before the catalog
is sealed. `register_complete` rejects an incomplete registration, and server
assembly stops before preflight. A capability omitted by the host availability
probe is different: the registration remains well formed but is excluded at
runtime with a reason naming the missing capability.

`/topologies` and `--list-topologies` project this sealed catalog and the same
availability probe used by selection. They do not maintain a separate list.

## Admission

Admission evaluates a graph against the coding contract and current host
capabilities. It checks the graph's input and output types, resource needs,
effects, and verification shape. Admission results are evidence records: a
refused candidate retains its own reason codes rather than disappearing from
the selection report.

Gate-planned graphs cross the same one-use native live-admission boundary as a
catalog registration's finite recipe. Materializing a plan is not permission
to execute it.

## Selection

The host builds candidate facts from the task profile, catalog snapshot,
admission results, configured policy, and any applicable outcome statistics.
Mechanically excluded candidates receive no score. Eligible candidates are
ranked by the Cosmos selector, and the recorded `SelectionDecision` is the
source for both execution and user-facing explanations.

A per-task topology directive can narrow selection to one registration or to
none. A run-wide topology pin supplies the default when a task has no directive.
Neither form falls back silently when its requested registration cannot run.

## Execution and evidence

The host readmits the selected graph immediately before execution so the
decision cannot rely on stale availability. It then executes at most one
registration for the task and records ordered evidence:

1. task profile and candidate facts;
2. admission evaluations;
3. selection decision and rationale;
4. graph execution observations;
5. verifier observations;
6. outcome observations appended after execution.

The prediction-aware edit topology records the expected post-edit content
before writing, then compares that prediction with the observed file. Other
registered shapes cover direct edits, worklist sweeps, coverage audits,
bounded tree questions, model-proposed edits, and the published default agent
loop. Availability determines which of these can run on a particular host.

Outcome statistics are frozen for a selection decision: observations appended
after one run affect only later tasks. This prevents the current execution from
rewriting the reasons that selected it.

## Error boundary

Cosmos APIs return the error forms owned by their respective subsystems rather
than one common error type. The coding host preserves those distinctions when
it reports catalog assembly, admission, selection, execution, verification,
and evidence failures.

## Verification

The contract is exercised by the workspace tests, especially:

- `crates/polaris_code_core/tests/cosmos_contracts_proof.rs`;
- `crates/polaris_code_core/tests/multi_pack_catalog_proof.rs`;
- `crates/polaris_code_core/tests/topology_selection_host_loop.rs`;
- `crates/polaris_code_core/tests/topology_directed_selection.rs`;
- `crates/polaris_code_core/tests/gate_plan_selection_host_loop.rs`.

Run them with `cargo +1.97.0 test --workspace`.
