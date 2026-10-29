# Integration Guide

`polaris_cosmos` is a library of domain-neutral cognitive models and world-model
parts expressed with Polaris graph primitives. It is not a standalone binary.
Agent builders compose its graph builders directly and add plugins only when a
selected model or memory surface needs server lifecycle integration.

## Current integration patterns

| Goal | Cosmos surface | Polaris contract |
|---|---|---|
| Build a prediction-error agent | `cognition::prediction_error_loop::build` | The resulting `Graph::signature()` |
| Build systematic coverage | `cognition::approved::coverage_sweep::build` and `prepare_context` | Native loops, branches, typed erased systems, and local resources |
| Build bounded correction | `cognition::approved::bounded_correction::build` and `prepare_context` | Native loops, switches, typed erased systems, and local resources |
| Identify the prediction-error topology | `prediction_error_loop::topology_key`, `TOPOLOGY_ID`, and `TOPOLOGY_VERSION` | Stable catalog identity |
| Describe an embodied prediction-error contract | `prediction_error_loop::contract_signature` plus an embodiment-owned `TopologyContractKey` | `GraphSignature::satisfies` |
| Register an embodied prediction-error graph | `prediction_error_loop::descriptor` plus `TopologyCatalog::register_complete` | Atomic native graph validation, stable signature explanation, and immutable candidate metadata |
| Assemble topology packs | `TopologyCatalogPlugin` plus pack plugins taking `Extends<TopologyCatalog>` | Capability-resolved startup extension, then a sealed global catalog |
| Observe cognitive roles | `OnPredict`, `OnAct`, `OnVerify`, `OnPredictionError` | Custom schedules through `HooksAPI` |
| Supply domain behavior | Pass typed stage systems and branch builders | Systems, `Out<T>`, `Res<T>`, `ResMut<T>` |
| Select compatible graph variants | Build candidate graphs with the same interface | `DynamicSlot` / `SubgraphRegistry` |
| Capture a portable task profile | `selection::transparent_profile` | Ordinary graph input/output and explicit provenance |
| Explain mechanical eligibility | `topology::preflight_catalog` | Catalog-owned stable signature explanations and declared requirements |
| Choose one registration | `selection::deterministic` | Complete immutable `SelectionInputs` and an explained decision |
| Record ordered evidence | `evidence::TopologyEvidence` | Replaceable provider contract; bounded in-memory default |
| Capture later statistics | `TopologyEvidence::capture_statistics` | Content-addressed contextual snapshot |

See the [cognitive-model guide](./cognitive-models.md) for model contracts and
the rest of this guide for the topology-runtime sequence.

## Author a topology pack

A **pack** is an ordinary embodiment plugin that contributes one or more owned
Polaris graphs to `TopologyCatalog`. It is a packaging convention: there is no
`Topology` or `TopologyPack` trait to implement. Start with the runnable,
self-contained [authoring fixture](../../tests/topology_pack_authoring.rs):

```sh
cargo test --test topology_pack_authoring --locked
```

The fixture registers two workflows under one embodiment-owned reporting
contract: a direct path reports the supplied integer unchanged; a conditional
path checks its sign and clamps negative values to zero. It tests both branch
outcomes, complete registration, missing-input admission, capability resolution,
and replacement of the catalog provider. Its `Request`, `Report`, and `Check`
are example application types, not Cosmos types to import.

### Dependency layout

This guide describes the exported Rust interfaces of `polaris_cosmos`, including
complete registration. The crate is not published to crates.io; consume it as a
path or git dependency.

Every direct dependency and transitive `polaris-ai` dependency must resolve to
one [Polaris](https://github.com/RadiantAILabs/polaris) source and version. In
this workspace that is the `polaris` entry under `[workspace.dependencies]` in
the root `Cargo.toml`. Verify from the consumer as well as Cosmos:

```sh
cargo tree -i polaris-ai --locked
```

The tree must identify exactly one `polaris-ai` package source. Equal crate
names and versions from different sources do not give you interchangeable
`Graph`, `Server`, or resource types.

### Choose the package boundary

Use a one-topology pack when a workflow has independent dependencies,
configuration, or installation policy. Group several topologies when they serve
a coherent task family, share domain ports and capabilities, and should be
installed together. The catalog remains heterogeneous: a pack can also contribute
different contracts when that grouping is useful. Each graph still gets its own
registration identity and metadata; a pack is neither a selection policy nor an
aggregate executable.

Keep task payloads, tools, model bindings, verifier logic, and environment
resources in the embodiment. A Cosmos model can supply reusable reasoning
structure while the embodiment supplies its typed systems. A direct pipeline
needs only native `Graph` construction. See
[Cognitive models](./cognitive-models.md#embodiment) when using a Cosmos builder.

### 1. Define the semantic port once

`TopologyContract::new` combines a stable versioned key, a native
`GraphSignature`, and task/result semantics. The fixture's `report_contract`
uses this boundary:

```rust
GraphSignature::new()
    .require_output::<Request>()
    .produce::<Report>()
```

`Request` arrives through `Out<Request>` after the host inserts it into the
execution context. A graph needing a typed resource instead declares it through
`Res<T>` or `ResMut<T>`; the corresponding contract can use `require_read::<T>()`
or `require_write::<T>()`. The graph's signature is derived from its systems.
The contract is the requested interface, not a handwritten copy of every node
or intermediate output. Registration checks `GraphSignature::satisfies` and an
explicit descriptor claim. Structural compatibility alone does not establish
that an implementation honors the documented result meaning.

Register a shared contract once, before its claiming graphs. Duplicate contract
keys are errors, even if their definitions look identical. When independent
packs share a port, give its registration to one common embodiment contract
plugin/capability and make the packs depend on that capability. Do not rely on
incidental plugin insertion order or swallow a conflicting contract definition.

### 2. Build native graphs and complete metadata

`registration(checked)` in the fixture builds either `graph.add_system(direct)`
or `check` followed by `add_conditional_branch`. Both produce `Report`, but their
reasoning and behavior differ. A new name for the same graph is not another
reasoning shape.

Complete registration takes these values together:

| Input | What the author supplies |
|---|---|
| `RegistrationSelectionRef` | Registration ID, topology key, revision digest, and resolved configuration |
| `TopologyDescriptor` | Topology key, title, summary, and explicit contract claims |
| `Graph` | The owned native graph; registration derives its signature |
| `RegistrationProvenance` | The stable identity of the contributing pack or embodiment |
| `CandidateRequirements` | Provider, binding, capability, and runtime-mechanism requirements beyond native typed ports |
| `TypeExplanationRegistry` | Stable keys for every boundary in the derived native signature |
| `Vec<ScoreComponent>` | Optional attributed author priors; the source must be `ScoreSource::AuthorPrior` |

The fixture finishes each input with the public constructor:

```rust
CompleteTopologyRegistration::new(
    selection,
    descriptor,
    graph,
    RegistrationProvenance::new("example.report-pack").unwrap(),
    CandidateRequirements::none(),
    explanations(),
    Vec::new(),
)
```

Use `none()` only when there are no additional runtime requirements. A pack
requiring a remote model, binding alias, or runtime mechanism must describe
those requirements with `CandidateRequirements::new`; native resource needs
come from the graph signature. The catalog is not a credential resolver.
The fixture has no external providers or secrets.

`TopologyKey` identifies a versioned reusable shape; `TopologyContractKey`
identifies a semantic typed port; `RegistrationId` identifies one concrete
embodied/configured graph. Different concrete subject types require compatible
concrete contracts, even when they share a generic model builder. Allocate new
registration IDs and revision digests for behavior/configuration changes.
Decide a topology-version change when graph logic, roles, termination, or
side-effect meaning changes. The catalog does not hash graph implementation
code: the author supplies a revision that tracks its implementation and build.
The fixture's fixed revision strings are test identities, not automatic source
fingerprints.

`TopologyDescriptor` currently has identity, title, summary, and contract
claims. Rich model-specific descriptors and documentation can explain fit,
anti-fit, roles, stop meanings, and limitations; do not invent extra methods on
`TopologyDescriptor`. Author priors remain attributed estimates, distinct from
measured outcomes. They cannot override mechanical exclusions.

### 3. Resolve configuration and explain types

Validate application-specific ranges and cross-field rules in the embodiment
before building its graph. `ResolvedConfigurationRef::inline` stores a schema
key and a canonical, secret-free `ConfigurationValue`; it checks the value and
its content identity, not the meaning of an arbitrary domain schema. Use
`ConfigurationEntry::new` and `ConfigurationValue::map` for sorted unique map
keys. The fixture records the immutable `checked` boolean under
`example.report-config@1.0.0`. Pass the same validated settings to graph
construction and metadata so they cannot drift.

Keep credentials, resource handles, task text, and provider-specific settings
out of these records. Use host-resolved binding aliases and safe provider
configuration digests where appropriate. A schema key alone does not install a
validator or resolve a binding.

Bind every native boundary, including intermediate outputs exposed by the
signature, using typed calls such as:

```rust
types.bind_output::<Request>(type_key("example.request"))?;
types.bind_output::<Report>(type_key("example.report"))?;
types.bind_output::<Check>(type_key("example.check"))?;
```

`bind_resource::<T>` covers resource boundaries. Stable `ContextTypeKey` values
explain types for admission and history; they never satisfy native Rust type
requirements. The catalog merges these registries into one authoritative
registry. Conflicting native/stable bindings fail; repeating the same binding
is safe. Agree on shared port keys across packs. For Cosmos-owned private model
state, use the model's `register_type_explanations` helper as described in the
[cognitive-model guide](./cognitive-models.md#explaining-boundaries-so-a-point-can-be-selected).

### 4. Contribute through capabilities

The fixture's ordinary plugin extends the catalog and requires an independent
embodiment settings capability:

```rust
struct ReportPack;

#[plugin(id = "example::report_pack", version = "1.0.0")]
impl Plugin for ReportPack {
    fn build(
        &self,
        mut catalog: Extends<TopologyCatalog>,
        _settings: Requires<ReportSettings>,
    ) {
        catalog.register_contract(report_contract()).expect("unique report contract");
        catalog.register_complete(registration(false)).expect("complete direct graph");
        catalog.register_complete(registration(true)).expect("complete checked graph");
    }
}
```

The helper functions and all imports appear in the linked executable fixture.
`ReportSettings` implements Polaris `Contract` and `LocalResource`; its provider
advertises `provides(ReportSettings)` and inserts the build resource. Model,
memory, and tool capabilities can follow the same typed dependency pattern.
Use `Requires<T>` for read access and `Extends<T>` for build-time extension;
depend on capability contracts rather than a particular provider plugin ID.
A build-only dependency need not become a runtime graph resource. If graph
systems use that resource at runtime, declare and supply it there too.

Install the pack, settings provider, and one catalog provider, then call
`server.finish().await`. Capability resolution orders build steps even when the
pack was added first. A missing required capability fails assembly before its
build step. The fixture uses `expect` for fixed, tested startup definitions;
handle invalid user configuration before assembly rather than silently omitting
a failed registration.

`TopologyCatalogPlugin` inserts a mutable catalog during `build`, then audits
`seal_admission_ready` and globalizes the sealed catalog during `ready`.
`register_complete` validates a single graph and metadata atomically: a failed
call leaves registrations, type bindings, and generation unchanged. A whole
pack's multiple calls are not one transaction. Propagate/fail on errors; do not
continue with an accidentally partial pack.

The complete path rejects unexplained boundaries during registration. Partial
`register` entries cannot pass the provider's completeness audit. The catalog is
startup-sealed: replacement means assembling another compatible provider or
choosing another registration, not mutating graphs in a live sealed catalog.
The fixture's `AlternateCatalog` implements the same build/seal/global lifecycle;
the unchanged pack produces the same snapshot content digest with either
provider. Install one catalog provider per assembly.

### 5. Preflight, then hand off to the host

After assembly, obtain `server.get_global::<TopologyCatalog>()`, call
`catalog.snapshot()`, and retain its generation and `content_digest`. Build a
`RuntimeAvailabilitySnapshot` from actual host observations, including required
resources/access modes, upstream outputs, and declared provider/binding facts.
The fixture's `availability(has_request)` shows the complete construction for a
host-supplied `Request`; its fixed UUID and time are test data. Production hosts
allocate fresh event IDs and report freshness truthfully.

```rust
let admissions = preflight_catalog(
    &snapshot,
    &contract_key(),
    &availability,
    false, // the host has established that this observation is fresh
)?;
```

Every captured candidate gets an evaluation. Missing `Request` excludes both
fixture graphs with `upstream-output.missing`; stale observations exclude them
with `availability.snapshot-stale`. No parallel candidate/type/schema registry
is needed for this preflight path: the complete snapshot owns that metadata.
Only mechanically admitted candidates proceed to hard policy eligibility and
ranking; preflight itself neither selects nor runs a graph.

Preflight is not permission to execute. Follow the topology-runtime sequence
below: preserve the complete selection decision, resolve at the captured
generation, and call `readmit_live` with the actual pristine context and current
availability. Use the captured candidate's admission metadata to construct its
`AdmissionCandidate`, the catalog's contract and `type_explanations()`, and the
host's known configuration schemas. Do not invent a second source of metadata.
Consume the resulting `StartPermit` for that decision, registration, and context
before the host calls `GraphExecutor`. Stable keys and optimistic availability
cannot substitute for missing native resources or outputs. Never automatically
try another topology after execution has started.

### Test the pack as a consumer

Keep authoring tests outside the crate's internal modules and import only
exported interfaces. Test actual behavioral differences, every meaningful
branch, contract satisfaction, missing boundary explanations, rejected claims,
capability failures, availability exclusions, and catalog-provider replacement.
For models with local state, create a fresh context and call their
`prepare_context` for each run. Assert semantic results separately from graph
completion. The fixture's direct executor calls are graph behavior tests; they
are not a production selection host.

Run `cargo fmt -- --check`, `cargo check --all-targets`,
`cargo test --all-targets`, `cargo clippy --all-targets -- -D warnings`, and the
single-source dependency check above. The standalone fixture is the small
starting point; [the vertical test](../../tests/topology_runtime_vertical_slice.rs)
covers selection, live admission, execution, and separate outcome evidence.

## Topology-runtime sequence

The host-facing flow is deliberately explicit:

1. Register owned graphs with `TopologyCatalog::register_complete`, supplying
   one `RegistrationSelectionRef`, `CandidateRequirements`, attributed author
   priors, and a `TypeExplanationRegistry` that binds every native signature
   boundary. Registration inserts the graph and metadata together or not at
   all. `TopologyCatalogPlugin` audits completeness before it seals the catalog.
2. Retain the sealed snapshot's `content_digest`, then project host
   observations into `RuntimeAvailabilitySnapshot` and call
   `preflight_catalog`. Configuration schema identity, stable signature
   explanation, requirements, and the complete candidate universe come from
   the snapshot rather than host-maintained parallel maps.
3. Aggregate `TaskProfileEvidence`, construct an explicit `Objective`, capture
   a frozen `TopologyStatsSnapshot`, and invoke the deterministic selector.
4. Persist `SelectionRecord` before touching a live graph. Resolve the selected
   `RegistrationId` at the captured catalog generation, repeat admission against
   the pristine `SystemContext`, and consume the one-use `StartPermit` at the
   start boundary.
5. The external harness appends `RunOpened`, executes the resolved native
   `Graph` with `GraphExecutor`, and appends mechanical finalization. Completion
   is not a correctness judgement.
6. Append delayed or conflicting `OutcomeObservation` values without rewriting
   the run or earlier observations, then capture another contextual statistics
   snapshot for a later selector invocation.

Catalogs and selectors do not execute graphs; Cosmos does not own credentials,
provider binding, pristine-context construction, retry after side effects, or
host/session lifecycle. The end-to-end fixture in
`tests/topology_runtime_vertical_slice.rs` demonstrates this boundary with
direct, prediction-error, coverage, and bounded-correction registrations.

`TopologyCatalog::register` and `preflight_candidates` remain compatibility
surfaces for direct low-level fixtures. A catalog containing one of those
partial registrations cannot pass `seal_admission_ready`, which is the
boundary used by the default provider. Production pack plugins should use only
the complete path; a missing type explanation is an assembly error, not a
runtime exclusion.

## World-model patterns

This crate does not include a world-model backend. A world model is meant to be
built from these parts:

| Goal | Parts to combine |
|---|---|
| Hold a queryable world model | A memory backend |
| Split memory into tiers | A backend plus a store-layout policy |
| Project memory into context | A backend plus a recall policy |
| Fold observations into memory | A backend plus a consolidate policy |
| Propose a schema change | A rewire policy plus the Polaris graph-mutation bridge |
| Join delayed outcomes to actions | A credit-assignment policy |

Every backend, layout, channel, and policy remains a point on an open axis.

## Adding a surface

- **Cognitive model:** follow [Cognitive Models](./cognitive-models.md), add the
  model to `src/docs/models.md`, and document its signature, roles, embodiment
  ports, state lifetime, policies, and behavioral tests.
- **Plugin:** follow the [Plugin Documentation Standard](./plugins.md), document
  provided capabilities and lifecycle, and add it to `src/docs/plugins.md`.
- **API:** follow the [API Documentation Standard](./api.md), classify its
  composition policy, and add it to `src/docs/apis.md`.
- **Resource:** follow the [Resource Documentation Standard](./resources.md),
  document scope and access, and add it to `src/docs/resources.md`.

Use the [taxonomy](../taxonomy.md) to decide whether logic belongs in Polaris
graph mechanics, Cosmos cognition, or a domain embodiment.
