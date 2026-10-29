---
title: polaris_cosmos Taxonomy
---

# polaris_cosmos Taxonomy

`polaris_cosmos` is the domain-neutral cognitive-model library, topology
discovery layer, and world-model substrate for Polaris agents. For the design
philosophy these layers serve, see the shared [philosophy](./philosophy.md).

## Ownership boundary

| Concern | Owner |
|---|---|
| Systems, graphs, signatures, execution, resources, hooks, and lifecycle | `polaris` |
| Domain-neutral topology identities, contract/catalog types, discovery, and selection policy | `polaris_cosmos` |
| Domain-neutral reasoning models, memory, learning, credit, and adaptation policy | `polaris_cosmos` |
| Embodiment-specific contracts, concrete topology registrations, domain actions, observations, tools, verifiers, and environment integration | Embodiments such as `polaris_code` |

The boundary follows a portability test: if logic remains meaningful when
"edit code and run tests" becomes "move a robot and inspect a camera" or
"research a question and check sources," it belongs in Cosmos. Files, IDEs,
compilers, repositories, and code-specific scoring belong in `polaris_code`.

Polaris owns the graph *mechanism*. Cosmos owns reusable cognitive graphs and
the semantic discovery substrate built with that mechanism. Embodiments fill
the graphs' task-specific ports and register the resulting concrete graphs.

## The agent model

A self-improving agent is `M = (G, W)`:

- **Schema (`G`)** is how the agent computes: a Polaris graph of systems and
  control flow. Polaris supplies the representation and executor; Cosmos may
  supply many domain-neutral graph schemas.
- **Memory (`W`)** is what the agent knows and has learned: facts, skills,
  learned parameters, and experience. Cosmos owns its domain-neutral contract
  and implementations.

Three coupling operations connect them:

- **Recall** projects relevant memory into a running schema.
- **Consolidate** folds observations and errors back into memory.
- **Rewire** proposes a schema change as a function of memory and feedback.

Credit assignment joins an action with a later outcome so consolidation or
rewire can attribute what happened. These operations are open policy slots;
their implementations are points, never universal definitions.

A topology catalog is not a third component added to `M`. It is a construction
and selection substrate over possible values of `G`. Once a registration is
selected and resolved, the running agent is still `M = (G, W)`: the selected
Polaris graph is `G`, and all per-run mutable state lives in its execution
context or in `W`. Historical topology evidence may later inform selection or
be consolidated into `W`, but the catalog itself is not agent memory.

## The cognitive-model unit

A cognitive model is a named, reusable point in cognitive-architecture space,
expressed as ordinary Polaris graph systems. Cosmos does not define a parallel
executor or universal `CognitiveModel` trait.

Its **structural contract** is the graph's derived `GraphSignature`:

- `requires`: external `Res<T>` / `ResMut<T>` dependencies;
- `requires_outputs`: upstream `Out<T>` inputs;
- `produces`: outputs emitted by the graph.

`GraphSignature::compatible_with` checks exact interfaces for dynamic
substitution. `GraphSignature::satisfies` checks named capabilities by
subsumption. A signature proves structural compatibility, not meaning: shared
payload types, capability names, role schedules, documentation, and behavioral
tests carry the semantic contract.

Complete cognitive models may implement Polaris's existing `Agent` trait.
Composable models may instead build into a caller's `Graph`. Agent builders can
select models statically or place signature-compatible variants behind a
`DynamicSlot`.

No cognitive model is canonical. ReAct, planner/executor, prediction-error,
reflection, search, debate, or future models may coexist in the library.

Cosmos's topology catalog makes those points discoverable without changing the
unit. A `RegisteredTopology` is an immutable embodied `Graph` plus its derived
signature, stable semantic descriptor, and provenance. `TopologyContract`
couples a stable semantic name to a native signature; it does not introduce a
universal model trait or a second structural compatibility relation.

## The topology-discovery unit

Topology discovery separates a reusable graph shape from its concrete embodied
registrations and from individual executions:

| Concept | Meaning | Lifetime and owner |
|---|---|---|
| **Topology definition** | A reusable, usually generic graph builder such as `prediction_error_loop::build` | Cosmos Layer 2; process-independent source definition |
| **`TopologyContract`** | A stable semantic key coupled to a native `GraphSignature` and documented task/result meaning | Cosmos Layer 1 type; the layer defining the semantic port owns its value—usually an embodiment for domain task ports, or Cosmos for domain-neutral ports |
| **`TopologyDescriptor`** | Canonical identity, version, title, and summary coupled to the contracts claimed by one registration | Layer 1 value assembled from model-owned metadata and embodiment-owned claims; immutable after registration |
| **`RegisteredTopology`** | An owned, validated, embodied `Graph` plus its derived signature, descriptor, configuration digest, and provenance | Constructed by a Layer 3 topology pack; retained for one catalog generation |
| **`TopologyCandidate`** | Selection-facing registration summary containing metadata and signature but no executable graph handle | Immutable catalog snapshot |
| **Execution** | A resolved registered graph running with a fresh context and run ID | Polaris runtime; one run, not a catalog type |

The stable identities answer different questions:

- `TopologyId`: which reusable cognitive shape is this?
- `TopologyContractKey`: which versioned semantic and typed port does it
  satisfy?
- `RegistrationId`: which concrete embodied graph and configuration can the
  runtime resolve?
- `ConfigurationDigest`: which immutable configuration produced that
  registration?

Generic system and payload types exist only while an embodiment builds the
graph. Registration erases those Rust construction parameters into the owned
`Graph`, its derived `GraphSignature`, and stable metadata. The catalog therefore
remains heterogeneous and does not require a universal `Topology` or
`TopologyFamily` trait.

`TopologyCatalog` is a startup-populated, process-wide Global resource. It
validates owned graphs, derives signatures, admits contract claims with
`GraphSignature::satisfies`, and then seals. Selection consumes an immutable
`TopologyCatalogSnapshot`; after selection, the runtime resolves the chosen
`RegistrationId` against the captured generation. The shipped deterministic
selector is an ordinary graph and policy function, not a method hidden inside
the catalog resource.
`TopologyCatalogPlugin` owns the default mutable-build/sealed-global lifecycle;
embodiment packs name the `TopologyCatalog` capability through `Extends` rather
than depending on that concrete provider.

## `prediction_error_loop`

One library point is `cognition::prediction_error_loop`:

```text
initialize → loop {
    predict → act → verify → compare → gate ─┬─ accept
                                               └─ reject
}
```

Cosmos fixes this model's logical topology and semantic roles, not its payloads.
The embodiment supplies typed systems for all five stages. `initialize` and
`compare` produce the same feedback type: initialization satisfies Polaris's
pre-iteration termination check, while each comparison supplies the gate and
the following termination check.

| Role schedule | Meaning |
|---|---|
| `OnPredict` | Commit an expected outcome before acting |
| `OnAct` | Act through the embodiment; the credit anchor |
| `OnVerify` | Observe or score the actual outcome |
| `OnPredictionError` | Compare verified and predicted outcomes |

The accept and reject branches are policy slots. They may consolidate memory,
record a veto, update credit, or propose rewire, but the model requires none of
those representations.

The catalog-facing metadata follows the discovery unit above:

| Surface | Ownership and meaning |
|---|---|
| `TOPOLOGY_ID` / `TOPOLOGY_VERSION` | Cosmos-owned stable identity of the reusable prediction-error shape |
| `topology_key()` | Versioned identity used by every embodied registration |
| `descriptor(contract_key)` | Canonical model metadata plus an embodiment-owned contract claim |
| `contract_signature::<P, A, V, F>()` | Native structural portion of the concrete embodiment contract |
| `build(...)` | Constructs the owned graph that a topology pack may register |

Coding and research embodiments therefore share one topology identity while
using different contract and registration identities. An execution of either
registration receives fresh context; it does not mutate the catalog entry.

## Approved coverage and correction points

`cognition::approved::coverage_sweep` and
`cognition::approved::bounded_correction` add systematic sequential coverage
and bounded iterative refinement as separate Layer 2 points. Both assemble
native Polaris control flow from typed embodiment systems, install only
per-run local state, retain explicit oracle qualification and residual
accounting, and advertise named alternatives. A direct routed registration
remains an embodiment pattern, keeping the low-overhead end of the topology
axis open without inventing a universal direct-model abstraction.

## Admission, selection, and evidence

Mechanical admission is Layer 1 composition logic over native derived
signatures. `TypeExplanationRegistry` projects process-local native types into
stable diagnostic keys, but never uses those keys to satisfy a live type.
`GraphExecutor::validate_resources` remains authoritative immediately before
the host starts the generation-checked registration.

Profiling and deterministic selection are Layer 2 policy points. They consume
explicit immutable values and emit ordinary typed outputs. The catalog neither
ranks nor executes candidates, and a mechanically excluded registration never
receives a score, rank, fallback position, or non-zero propensity.

`TopologyEvidence` is a replaceable host-neutral provider boundary for ordered
selection, admission, run, outcome, and credit events. Contextual statistics
retain candidate-set zero-run cohorts and, for a started selected candidate,
the embodiment and provider/model-family bucket from `RunOpened`. They are
selection evidence, not agent memory `W`; a later consolidation policy may
choose to project them into `W` once that separate contract exists.

## Cognitive and world-model axes

The broader design remains an open basis of independently settable axes:

| Axis | Spans | Mechanism / policy owner |
|---|---|---|
| Schema representation | Serializable and executable graph shapes | Polaris mechanism |
| Cognitive topology | Direct pipelines through loops, search, panels, and future graph models | Cosmos model points over Polaris graphs |
| Topology discovery | One hard-coded graph through a heterogeneous, sealed catalog | Cosmos Layer 1 over native signatures |
| Topology selection | Static choice through objective-weighted or learned selector graphs | Cosmos Layer 2 policy |
| Runtime substitution | Fixed graphs through signature-compatible `DynamicSlot` candidates | Polaris mechanism; Cosmos or host policy |
| Memory backend | Retrieval-only through predictive or parametric stores | Cosmos |
| Store layout | One store through multi-tier consolidation topologies | Cosmos |
| Recall | Memory-to-context projection policy | Cosmos |
| Consolidate | Observation and feedback write policy | Cosmos |
| Rewire | Memory-and-feedback-to-schema proposal | Cosmos |
| Gate | Accept, reject, defer, or veto an update | Cosmos model/policy |
| Trigger and termination | Event, cadence, budget, or learned stopping | Cosmos model/policy |
| Credit | Join actions to delayed outcomes | Cosmos |
| Verification | Domain-neutral role and protocol | Cosmos contract; embodiment signal |
| Task or curriculum | Fixed, generated, or learning-progress tasks | Embodiment input; Cosmos policy may select |
| Population or archive | One candidate through population search | Cosmos |
| Multi-agent topology | One agent through collaborators or opponents | Cosmos over Polaris graphs |

Every shipped model or policy is one point on these axes. Adding
`prediction_error_loop` does not privilege prediction-error cognition over
another model.

## Crate layers

| Layer | Name | Owns |
|---|---|---|
| **3** | Embodiment and lifecycle surfaces | Concrete registrations, topology packs, catalog/runtime plugins, domain adapters, rewire application, live observers |
| **2** | Cognitive models and world-model policies | Model and selector graphs, memory backends, recall/consolidate/rewire/credit policies |
| **1** | Composition and discovery contracts | Native graph signatures, stable topology identities, semantic contracts, descriptor/catalog/snapshot types, shared payloads, role schedules |

Layer 1 contains no cognitive hypothesis: graph signatures describe typed
inputs and outputs, while semantic types, stable identities, catalogs, and
schedules make composition and discovery explicit. Layer 2 contains swappable
hypotheses, including the policy that selects another topology. Layer 3 fills
generic ports, contributes concrete registrations, and connects selected points
to hosts and domain embodiments.

The layer of a type and the layer that supplies one of its values may differ.
`RegisteredTopology` and `TopologyCatalog` are Layer 1 substrate types; an
embodiment topology pack constructs registration values at Layer 3. Likewise,
`TopologyDescriptor` is a Layer 1 representation whose canonical identity and
human metadata live beside its Layer 2 model definition, while its concrete
contract claims come from the Layer 3 embodiment.

## Boundary guide

| Question | If yes |
|---|---|
| Is it graph execution, context flow, structural validation, or generic lifecycle? | `polaris` |
| Is it domain-neutral reasoning, learning, memory, control, or adaptation? | `polaris_cosmos` |
| Does it mention files, repositories, IDEs, compilers, coding tools, or code-specific correctness? | `polaris_code` |
| Does it define a structural interface between graphs? | Use `GraphSignature`; do not create a parallel contract system |
| Does it define stable topology identity, semantic discovery metadata, or catalog admission? | Cosmos Layer 1; retain native graph/signature authority |
| Does it define one reasoning topology or policy? | Layer 2 model point; keep the axis open |
| Does it choose among registered topologies? | Layer 2 selector graph or policy; do not hide cognition in a catalog method |
| Does it fill generic model ports with task-specific systems and payloads? | Embodiment; contribute the resulting concrete registration at Layer 3 |
| Does it package a selected model for a long-lived host? | Layer 3 plugin or adapter |
