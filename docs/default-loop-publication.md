# Publishing the default agent loop in the topology catalog

Code: `crates/polaris_code/src/cosmos/default_loop.rs`.
Proof: `crates/polaris_code/tests/default_loop_publication.rs` (10 tests).

The loop `CodeAgent` builds serves ordinary turns, but without a catalog
registration the catalog cannot name it. Every other registration in the catalog
was a coding topology; the thing that actually ran, most of the time, was
invisible to the surface that lists what can run.

The first question is whether the existing coding contract admits the default
loop; the answer determines what the published registration must claim.

## 1. The coding contract cannot admit the default loop

Asked and answered before anything was changed, and confirmed against the real
graph rather than read off the source.

`CodeAgent::build` produces a graph that validates standalone (0 errors, 0
warnings). Its derived `GraphSignature` is:

- **23 resource accesses over 18 types** — five of them (`AgentLlm`,
  `ContextManager`, the live transcript, the per-turn `PolicySnapshot`, and
  `ProjectContext`, which the turn boundary re-reads) are both read and
  written, and `satisfies` compares access modes exactly, so each direction is
  its own access.
- **`requires_outputs = {CaughtError}`** — the error handler reads an output no
  node in the graph produces, which is exactly what a free upstream output is.
- **`produces = {LlmResponse, LoopState, PreparedContext}`**.

The coding contract (`cosmos::topology::CONTRACT`,
`polaris-code.coding-assessment`) requires a graph that produces an
`AssessmentConclusion` and sanctions no free output reads. `satisfies_diff`
reports two independent violations:

| Axis | Value |
|---|---|
| `missing_produces` | `AssessmentConclusion` |
| `extra_requires_outputs` | `CaughtError` |

Neither can be changed without altering the default loop or the coding
contract:

- Producing an `AssessmentConclusion` would mean adding a node to the loop —
  restructuring the thing the task forbids restructuring.
- Removing the `CaughtError` read would mean deleting the loop's error handler.
- Widening the coding contract to sanction a free `CaughtError` read, or to
  drop the conclusion requirement, would change a contract six registrations
  already satisfy — and admission is supposed to decide, not be argued with.

So the answer to "does the contract admit it" is **no**, and the diagnosis is
executable rather than prose:
`default_loop::assert_default_loop_does_not_satisfy_the_coding_contract`
returns both diff axes, and the proof additionally drives a real
`register_complete` that fails with `TopologyCatalogError::UnsatisfiedContract`
naming the same diff.

## 2. What was published instead

A second pack plugin, `DefaultLoopTopologyPackPlugin`, owning its own contract:

```
contract  polaris-code.conversational-turn
requires  read UserIO, write LoopState
reads     CaughtError (free upstream output)
produces  LlmResponse
```

This is not a loophole around admission. A contract is the *statement of what a
slot needs*; the coding contract states what a coding assessment needs, and a
conversational turn is a different slot. The catalog already supports more than
one contract, and `host::read_catalog` finds the coding contract by ID, so a
second contract in the same catalog is inert for the coding path by
construction.

The registration is complete on the same terms as the six coding ones: a
selection reference, a configuration schema, a requirements set, a stable
signature explanation covering all 22 accesses and all four outputs, and
author priors.

**The graph it registers is `CodeAgent::build`'s, verbatim** —
`default_loop_graph()` is three lines and calls the agent. This was the
deliverable most at risk: two definitions of the default loop would have been
worse than none. `the_default_loop_is_defined_in_exactly_one_place` enforces it
structurally (exactly one `add_loop::<LoopState` site under `src/`, and it is
`agents/code_agent.rs`; the publication module contains no `add_loop`,
`add_conditional_branch`, or `add_error_handler`) and behaviourally (the
registered signature equals the agent's, equals the catalog-held one).

## 3. What the registration claims about itself, and why

Priors, capabilities, and availability are not obvious for this registration,
so each is stated rather than guessed.

- **Author priors: none.** An author prior is a claim about how well a topology
  performs, and nobody has measured the default loop against anything, because
  until now there was nothing to measure it against. Inventing a plausible
  number would be misleading because it would
  become the baseline every future comparison is scored against. The three
  prior-free coding registrations (`TEST_GATED`, `MODEL_PROPOSED`,
  `COVERAGE_AUDIT`) already establish that an empty prior set is legal.
- **Requirements: one provider.** A turn cannot be served without a model
  provider, so the registration declares a `ProviderRequirement` on the same
  `MODEL_PROVIDER` key the coding pack uses. Nothing else about a turn is a
  capability the host either has or lacks.
- **Availability: probed, not assumed.** `probe_availability` takes a
  `ConversationalHost { session_resources, model_provider }` and reports 18
  resource facts, one upstream-output fact, and the provider fact only when the
  host actually has one. Both answers are proved: a wired host admits it with
  zero exclusions; a hermetic one is refused with `provider.unavailable`, 18
  `resource.missing`, and `upstream-output.missing`.

## 4. Publication is not activation

Publication changes which path serves a turn for **no** input.

- The binary requests exactly one contract, the coding one, and only when the
  `CodingTaskQueue` has an accepted task. `host::select_and_run` returns
  `Ok(None)` before reading the catalog when the queue is empty, so an ordinary
  turn never consults the catalog at all.
- Under the coding contract the default loop is excluded mechanically with
  `contract.not-requested` and is never scored.
- With no priors there is nothing to rank it with even if some future contract
  did request it.

`publishing_the_default_loop_does_not_change_a_conversational_turn` drives the
same turn against the pre-task assembly and the post-task assembly and compares
the reason, final text, turn number, node count, whether a topology was
selected, whether a run started, the serialized usage record, and the canonical
transcript. (The run id and wall-clock duration are excluded: both are freshly
minted per execution.)

## 5. The loop never becomes unreachable

Two cases are proved rather than argued: a sealed but empty catalog, and a
catalog where every registration under every contract is excluded (the two
availability snapshots are deliberately crossed so nothing anywhere admits). In
both, an ordinary turn still completes with the same answer and selects no
topology.

## Findings

- **`candidate_facts` asserts a verifier capability it cannot know.**
  `cosmos::policy::candidate_facts` attaches `VerifierCapability::Deterministic`
  to *every* candidate in the snapshot. That is true of the six coding graphs
  and false of the default loop, which verifies nothing. It is harmless today
  because the default loop is never scored under the coding contract, but any
  host that ranks conversational registrations would be ranking on a fact
  nobody established.
- **A pack cannot contribute to another pack's contract.** Recorded already in
  [`cosmos-contracts.md`](cosmos-contracts.md); it is the reason the default
  loop ships as its own pack with its own contract rather than as a seventh
  entry in the coding pack.
- **No framework or TUI change was required.** The publication is implemented
  by the coding host, and `tests/cosmos_contracts_proof.rs` remains unchanged.
