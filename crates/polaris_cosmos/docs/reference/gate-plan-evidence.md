# Gate-plan evidence

`composition::GatePlan` is planner data, not a serialized native graph. A host
resolves it against a sealed catalogue, materializes an ordinary Polaris graph,
validates it, and records the resulting decisions through the composition
evidence contracts.

## Record identity

`contracts::GatePlanRecord` is attached to a composition decision, validation,
run, and outcome when the path came from a gate plan. It retains:

- the complete canonical plan digest, including configuration values;
- the catalogue snapshot digest used to resolve the plan; and
- a canonical expression-shape key that excludes configuration values and
  plan-local instance labels while retaining plan-level wires and their stable
  ports.

Records without a gate plan omit this optional metadata. Their wire encoding
and content digest do not depend on it, so a durable store can read records
with and without it together.

## Revision ordering

`CompositionRevisionEvidence` joins one decision to its following validation.
`CompositionRevisionTrail` begins with revision attempt one and orders later
pairs by consecutive one-based `RevisionAttempt` values. Every pair before the
final pair must be refused. A trail may then contain at most one run, and only
when its final validation is admitted; an outcome, when present, must follow
that run. This rejects a run after a refusal unless a later whole-plan revision
was admitted.

Refused validations retain owned `RecordedRefusal` values. They mirror the
materializer's stable code, gate path, key, expected and actual facts, and
repair hint, so the planner can submit a complete revised plan. A refused
gate-plan validation requires at least one such refusal; a validation without
a gate plan may retain an empty list.

## Querying outcomes

`evidence::GatePlanOutcomeIndex` is a caller-owned in-memory index over valid
revision trails. `outcomes_by_plan` returns outcomes for an exact plan digest;
`outcomes_by_shape` groups plans that share gate-expression structure and
wiring while differing only in configuration or plan-local instance labels. It
is an index, not a second evidence store: the host evidence provider remains
authoritative for retention and lifecycle.

## Fresh graph admission

A graph from `composition::materialize` is not a registered topology, so it
cannot use catalog-backed `readmit_live`. Call
`topology::readmit_materialized_graph` with the native graph, executor, and
pristine context immediately before execution. It repeats native structural and
live-resource checks and returns a one-use `MaterializedStartPermit` bound to
that graph allocation, decision, attempt, and context.
