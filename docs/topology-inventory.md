# Asking what this build can run

`polaris-code` registers its topologies at compile time and seals the catalog
at `ready`. Until now the only place that list was written down was
`register_coding_topologies`, a non-public function in
`crates/polaris_code_core/src/cosmos/topology.rs`. A user who wanted to know
what this build has — and whether each registration could actually run on this
machine — had to read Rust.

Two surfaces now answer that, and both derive the answer.

## Interactive: `/topologies`

```
/topologies
```

Replies in the conversation panel with every registration the sealed catalog
holds, what each one is for, and whether it can run here. Registered by
`crates/polaris_code_core/src/terminal/topologies.rs` on the same additive
`CommandRegistry` every other slash command uses.

## Scripted: `--list-topologies`

```bash
polaris-code --list-topologies
polaris-code --list-topologies --output-format json
```

Honours `--output-format`. `text` is the same rendering `/topologies` replies
with; `json` and `stream-json` both emit one JSON object, because a listing is
a value, not a message stream. Exits `0`.

`--list-topologies` conflicts with `-p/--print`: listing is not a turn. It
builds a server holding only the topology packs — `TopologyCatalogPlugin`,
`CodingTopologyPackPlugin` and `DefaultLoopTopologyPackPlugin`, the same three
`main.rs` gives the agent server — so it needs no `ANTHROPIC_API_KEY`, starts
no agent, and opens no session.

Sample output on a hermetic host:

```
8 topologies registered, 5 runnable here (catalog generation 10)

code.direct-edit  [runnable]
  Direct edit — One read, one exact-string write, one verification; lowest latency and no second look.
  registered by polaris-code.coding-topology-pack

code.test-gated-edit  [not runnable]
  Test-gated edit — Edits and gates the verdict on a real project test run; needs an installed test runner.
  registered by polaris-code.coding-topology-pack
  blocked: resource.missing (sound, but this host has not wired what it needs): polaris-code.project-test-runner
```

## Where each field comes from

Nothing in the listing is authored. `crates/polaris_code_core/src/cosmos/inventory.rs`
joins three things this build already had:

| Shown | Read from |
|---|---|
| which registrations exist | `TopologyCatalogSnapshot::candidates` |
| title, summary, claimed contracts | the registration's own `TopologyDescriptor` |
| who registered it | `RegistrationProvenance::provider` |
| can it run here | `preflight_catalog`, over `probe_workspace_availability` |
| why not | the `ExclusionReason`s Cosmos attached |
| catalog generation and digest | `TopologyCatalogSnapshot` |

Consequences worth stating:

- **One more registration appears with no edit to the listing.** Pinned by
  `an_invented_registration_appears_without_touching_the_listing` in
  `crates/polaris_code_core/tests/topology_inventory_listing.rs`, which invents
  a registration inside the test file — a name no shipped source can contain.
- **No description is invented.** Titles and summaries are the registration's
  own. A registration that carried none would list with none; the listing has
  nowhere to write one.
- **The listed verdict is the selected verdict.** `probe_availability`, which
  the live selection point in `src/cosmos/host.rs` calls, and
  `probe_workspace_availability`, which the listing calls, are one
  implementation — the former is the task-shaped call into the latter. There is
  no second probe to drift.
- **Listing is read-only.** `catalog.snapshot()` takes `&self`, the probe does
  one `metadata()` call on the workspace root, and `preflight_catalog` is pure.
  Listing does not register, unregister, re-seal, or execute anything.
  `listing_starts_nothing_and_writes_nothing` pins that the workspace is
  byte-identical afterwards.

The catalog stays sealed and registration stays compile-time. This surface
reads the list; it does not make it editable.

## Reading "cannot run here"

Cosmos emits fine-grained, stable exclusion codes. A user reading a list needs
the coarser question first, so `BlockerCategory` groups them — derived from the
code, never attached to a registration by hand:

| Category | Means | Codes |
|---|---|---|
| `defect` | this build registered it wrongly; no configuration will fix it | `type-explanation.missing`, `graph.invalid`, `contract.unsatisfied`, `registration.unknown`, `configuration.digest-mismatch`, `configuration.schema-unknown`, `catalog.stale` |
| `unobserved` | nothing here observed what it needs, so admission could not decide | `availability.unknown`, `availability.snapshot-stale` |
| `host-unwired` | sound, but this host has not wired what it needs | `resource.missing`, `resource.access-insufficient`, `upstream-output.missing`, `provider.unavailable`, `configuration.binding-unavailable`, `capability.missing`, `mechanism.unsupported` |
| `not-applicable` | runnable, but it answers a different contract | `contract.not-requested` |
| `unrecognized` | reported by a newer Cosmos than this host can classify | anything else |

A category alone does not distinguish two situations that share it, which is
why the blocker also carries the stable **code** and the stable **subjects**.
On a hermetic host, the two blocked coding registrations are both
`host-unwired`, and they are told apart by what they name:

- `code.test-gated-edit` — `resource.missing`, subject
  `polaris-code.project-test-runner`
- `code.model-proposed-edit` — `provider.unavailable`, subject
  `polaris-code.anthropic`

A failed probe is a third situation with the same category and the same code as
the first: `resource.missing`, subject `polaris-code.coding-workspace`. Only
the subject separates "no test runner installed" from "the workspace root is
not a writable directory", which is why the subject is shown and not just
categorised. `two_unwired_capabilities_are_told_apart_by_code_and_subject` and
`an_unobservable_workspace_reads_differently_from_an_unwired_capability` pin
both readings, and dropping the subjects makes both tests fail with
`[(HostUnwired, "resource.missing", [])]` — the collapse this design avoids.

An unrecognised code is shown with its subjects and marked `unrecognized`
rather than guessed at, so a Cosmos release that adds a code degrades to
"listed but unclassified" instead of being silently mis-explained.

## What the listing cannot tell you

`polaris_cosmos` records no "this registration exists to demonstrate an
exclusion" property, and neither does the coding pack. Every exclusion in this
build is a live observation about the host it ran on — see
[`cosmos-contracts.md`](cosmos-contracts.md#admission), where five
coding registrations are admitted and two excluded by capabilities the
hermetic host does not provide. A registration that exists solely to prove an
exclusion channel would therefore be indistinguishable here from one blocked by
a genuinely unwired host, and the listing does not claim otherwise. Making that
distinction visible needs the registration to carry the claim; that is a
`polaris_cosmos` change, not a listing change.
