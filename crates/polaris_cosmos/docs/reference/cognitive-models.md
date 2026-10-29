---
title: Cognitive Models
---

# Cognitive models

Cosmos cognitive models are reusable logical topologies built from native
Polaris graph systems. Agent builders select a model and supply its embodiment
systems, resources, and payload types.

## Contract

A model's derived `GraphSignature` is its structural contract. It records
required resources, required upstream outputs, and produced outputs. Cosmos
does not define another executor, graph interface, or universal cognitive
trait.

Signatures are in-process typed contracts based on `TypeId`. Rendered type names
are diagnostic only. Exact `compatible_with` matching governs `DynamicSlot`
substitution; `satisfies` governs named capability discovery.

A structural match does not imply semantic equivalence. A model's shared payload
types, capability name, role schedules, documentation, and behavioral tests
define what the graph means.

## Embodiment

An embodiment supplies task-specific work while Cosmos retains the reusable
logic. For `prediction_error_loop`, a coding embodiment might provide:

| Cosmos role | Coding embodiment |
|---|---|
| Predict | Predict whether a planned patch will satisfy tests |
| Act | Edit files or invoke a coding tool |
| Verify | Run tests, Clippy, or a domain-specific grader |
| Compare | Convert prediction and verified result into feedback |
| Accept/reject | Consolidate the lesson or record a veto |

The same topology can accept robotics, research, or operational systems with
different payloads.

## `prediction_error_loop`

Build the model into an agent graph with
`cognition::prediction_error_loop::build`. The `Stages` value supplies
`initialize`, `predict`, `act`, `verify`, and `compare` systems. `Policies`
supplies termination, gating, and accept/reject branch builders.

```rust,ignore
use polaris_cosmos::cognition::prediction_error_loop::{self, Policies, Stages};

prediction_error_loop::build(
    &mut graph,
    "learn",
    Stages::new(initialize, predict, act, verify, compare),
    Policies::new(terminate, gate, accept_branch, reject_branch),
);

let signature = graph.signature();
let contract_signature = prediction_error_loop::contract_signature::<
    Prediction,
    ActOutput,
    Verified,
    Feedback,
>();
assert!(signature.satisfies(&contract_signature));

let descriptor = prediction_error_loop::descriptor(embodiment_contract_key);
assert_eq!(descriptor.key(), &prediction_error_loop::topology_key());
```

`topology_key()` identifies the reusable cognitive shape. The embodiment owns
the `TopologyContractKey` because it chooses the concrete task/result semantics
and payload types. A topology-pack or application builds the graph, creates the
contract from `contract_signature`, and transfers both the descriptor and owned
graph into `TopologyCatalog::register_complete`, together with resolved
configuration, revision, requirements, and stable type explanations. Follow the
[standalone topology-pack authoring guide](./guide.md#author-a-topology-pack)
for the complete plugin recipe and executable fixture.

`initialize` and `compare` must produce the same `Feedback` output type.
Polaris checks a loop predicate before its first iteration, so initialization is
part of the model rather than an undocumented caller precondition. The gate and
termination predicates both read `Feedback`.

The other stage outputs are embodiment-defined. Their downstream dependencies
remain ordinary `Out<T>` system parameters and are checked by Polaris resource
validation.

## `coverage_sweep`

`cognition::approved::coverage_sweep` is the systematic sequential-coverage
point. An embodiment supplies systems to scope subjects, probe and adjudicate
one subject, assess gaps, combine the complete ledger, and resolve terminal
residuals. `Config` bounds the subject set, ledger, and loop; `prepare_context`
installs fresh private worklist and ledger state for one run.

The descriptor records the oracle qualification, intended and anti-fit,
runtime mechanisms, author priors, stop reasons, and alternatives. A structural
contract can be derived with `contract_signature::<Subject, Observation>()`,
but the concrete semantic `TopologyContractKey` remains owned by the
embodiment.

```rust,ignore
use polaris_cosmos::cognition::approved::{OracleQualification, coverage_sweep};

coverage_sweep::prepare_context(
    &mut context,
    coverage_sweep::Config::new(32, 32, 32, StopReason::IterationLimit)?,
)?;
coverage_sweep::build(
    &mut graph,
    coverage_sweep::Stages {
        scope,
        empty_scope,
        probe,
        adjudicate,
        assess_gaps,
        combine,
        resolve,
    },
    OracleQualification::rejected_negative_control(negative_control),
);
```

## `bounded_correction`

`cognition::approved::bounded_correction` is the bounded iterative-refinement
point. Each round produces a subject, verifies it, routes the closed `Status`
vocabulary exhaustively, and records the assessment. Terminal accounting is
mandatory: normal and outward-escalated conclusions use distinct supplied
resolver systems, while the host retains ownership of the escalation action.

Use `Config::new` and `prepare_context` to install a fresh round controller and
bounded ledger. Its descriptor and `contract_signature::<Subject>()` follow the
same identity and embodiment boundary as `coverage_sweep`.

## Explaining boundaries so a point can be selected

Complete registration projects the native `GraphSignature` through a
`TypeExplanationRegistry` before the catalog accepts the graph. A boundary
without a stable key makes `register_complete` fail atomically; the default
provider also rejects partial registrations when sealing. The low-level
`preflight_candidates` path reports `type-explanation.missing`, but
production packs should catch this at assembly through complete registration.

Cosmos explains the boundaries Cosmos introduces; the embodiment explains its
own. Both approved points keep their worklist, ledger, control, and progress
types private, and `bounded_correction` routes the shared `Status` vocabulary,
so no consumer can name those types to bind them. Each point therefore ships:

- `register_type_explanations::<Subject>(&mut registry, &subject_key)` — binds
  every boundary the point introduces, keyed off the embodiment's own stable
  key for `Subject`. Binding is idempotent and the two points agree on the keys
  they share, so registering both against one subject is safe.
- `state_resource_keys(&subject_key)` — the stable keys of the state
  `prepare_context` installs, which a host reports as writable resources in its
  runtime availability snapshot. Without those facts an explained registration
  is still excluded, now with `resource.missing`.

The embodiment binds the types it owns: its subject and observation types, and
the `Assessment`, `Vec`, summary, and conclusion boundaries its own systems
produce.

```rust,ignore
let subject = ContextTypeKey::new(ContextTypeId::new("acme.criterion")?, version);
registry.bind_output::<Criterion>(subject.clone())?;
// … the embodiment's remaining boundary types …
coverage_sweep::register_type_explanations::<Criterion>(&mut registry, &subject)?;

for key in coverage_sweep::state_resource_keys(&subject)? {
    resources.push(ResourceAvailability::new(key, ResourceAccess::Write, observed)?);
}
```

## Direct routed registrations

A low-overhead direct pipeline is an embodiment pattern, not a Cosmos builder.
Build it from ordinary Polaris systems, attach a `TopologyDescriptor` and an
embodiment-owned contract claim, and register the owned graph beside reusable
Cosmos points through `register_complete`. The
[authoring fixture](../../tests/topology_pack_authoring.rs) pairs a direct graph
with a conditional checked graph under one contract, exercising both branch
outcomes and admission without a universal model trait. Promotion to a shared
builder requires demonstrated reuse across embodiments; a shared trait or second graph abstraction is not required.

## Profiling and selection points

`selection::transparent_profile` is the default explainable profiler point.
It consumes only the closed `TaskProfileEvidence` vocabulary and retains
missing values, conflicts, confidence, and policy provenance.

`selection::deterministic` is an ordinary fallible Polaris graph and callable
pure policy function. Its complete `SelectionInputs` contains the task profile,
objective, requested contract, immutable catalog and availability snapshots,
mechanical admission set, frozen contextual statistics, attributed candidate
facts, and selector configuration. Mechanical exclusions are authoritative;
only admitted candidates receive policy checks, scores, ranks, fallback
positions, or propensity. The result is either a fully explained choice or a
typed `NoEligibleCandidate` outcome.

## State and observability

Loop outputs share one context and overwrite by concrete type each iteration.
Use `Out<T>` for stage handoffs and a small `ResMut<T>` only for genuinely
accumulated session state.

The shared `OnPredict`, `OnAct`, `OnVerify`, and `OnPredictionError` schedules
re-emit system lifecycle events for their role. Observers can instrument the
model without knowing embodiment payload types or system function names.

## Adding a model

1. Start with the model's domain-neutral logical topology.
2. Express all execution and data flow with Polaris graph primitives.
3. Keep payload and resource types generic where meaning belongs to an
   embodiment.
4. Document the derived signature, semantic roles, state lifetime, policies,
   and expected failure behavior.
5. Prove meaningful branches and role schedules with behavioral tests.
6. Give the model a stable topology ID/version and descriptor helper without
   making catalog registration mandatory.
7. If the model introduces a boundary type an embodiment cannot name, ship a
   `register_type_explanations` helper for it and prove with a test that the
   model leaves no boundary of its own unexplained. A model that cannot be
   explained cannot be selected.
8. Add the model to `src/docs/models.md`.
