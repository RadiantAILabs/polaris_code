# Cognitive models

A catalog of the domain-neutral cognitive models shipped by `polaris_cosmos`.
Each model is a named point expressed as a Polaris graph; none is canonical
intelligence.

- [`prediction_error_loop`](crate::cognition::prediction_error_loop) — repeats
  predict, act, verify, and compare roles, then gates accept/reject policy
  branches. Agent builders supply every stage system and payload type. Its
  structural contract is the graph's derived
  [`GraphSignature`](polaris::graph::GraphSignature). `TOPOLOGY_ID`,
  `TOPOLOGY_VERSION`, `topology_key`, and `descriptor` provide canonical catalog
  metadata; `contract_signature` constructs the structural portion of an
  embodiment-owned semantic contract.

- [`coverage_sweep`](crate::cognition::approved::coverage_sweep) — scopes a
  bounded subject set, probes and adjudicates every visited subject, makes
  unvisited gaps explicit, then combines and resolves one immutable assessment
  conclusion. Its private worklist and ledger are per-run state, and its
  descriptor retains oracle qualification, intended fit, limits, and named
  alternatives.

- [`bounded_correction`](crate::cognition::approved::bounded_correction) —
  iterates a produce/verify/status-dispatch loop under explicit round and ledger
  bounds, then routes every terminal residual through resolve or outward
  escalation accounting. `Status` routing is exhaustive and the host owns any
  real escalation mechanism.

Both approved points keep their worklist, ledger, control, and progress state
private, so an embodiment cannot name those types to explain them. Each point
therefore ships `register_type_explanations` and `state_resource_keys` beside
`prepare_context`: Cosmos explains the boundaries Cosmos introduces, and the
embodiment explains its own subject, observation, assessment, summary, and
conclusion types. A pack that skips the registrar is rejected atomically by
`TopologyCatalog::register_complete`; the incomplete point never appears in a
sealed candidate snapshot.

## Selection policy points

- [`transparent_profile`](crate::selection::transparent_profile) — aggregates
  the closed portable task-profile evidence vocabulary without inventing
  values: missing and conflicting observations remain explicit and every
  aggregation decision retains policy provenance.

- [`deterministic`](crate::selection::deterministic) — hard-gates the complete
  mechanically admitted candidate set, scores only eligible registrations from
  attributed components, applies the objective's exact tie-break, and emits an
  explained selection or typed no-eligible result. It consumes frozen catalog,
  availability, and statistics snapshots and never resolves or executes a
  graph.

Direct routed graphs remain an embodiment production pattern rather than a
Cosmos model builder. The shared vertical fixture registers that pattern beside
the reusable points to prove they participate in the same contracts.

See the [cognitive-model guide](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/cognitive-models.md)
for composition, embodiment, signatures, state, and observability.
