A catalog of every [`API`](polaris::system::api::API) shipped by
`polaris_cosmos`. APIs are the typed handles plugins publish so other plugins
can contribute to or drive them at wiring time.

Each entry links to the API's rustdoc page, which documents its surface,
lifecycle, composition policy, and a provider+consumer example per the
[API documentation standard](https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/api.md#documentation-standard).

> **Drift guard.** This catalog is verified by an integration test
> (`tests/api_catalog.rs`) that scans `src/` for `impl API for X` and asserts
> each name appears below. Adding a new API without listing it here will fail
> CI.

> This crate currently defines no APIs.

## Flow construction

The [`flow`](crate::flow) module exposes ordinary Rust graph constructors,
not framework APIs. [`agent`](crate::flow::agent) builds a bounded role-call
subgraph and [`phase`](crate::flow::phase) wraps a body with its lifecycle policy.
[`TaskRunner`](crate::flow::TaskRunner) is the replaceable runtime boundary.

| Native surface | Consumer contract |
|---|---|
| [`agent`](crate::flow::agent), [`agent_roles`](crate::flow::agent_roles) | Build one bounded role-call graph and inspect its declared roles. |
| [`phase`](crate::flow::phase), [`phase_status`](crate::flow::phase_status) | Wrap a body with enablement, events and failure policy; project its recorded status. |
| [`memo`](crate::flow::memo), [`memo_current`](crate::flow::memo_current) | Restore typed completed outputs by a fixed or runtime step key. |
| [`repair_loop`](crate::flow::repair_loop), [`counter`](crate::flow::counter) | Compose check/repair edges over a shared durable cap. |
| [`lease_loop`](crate::flow::lease_loop), [`rounds`](crate::flow::rounds) | Compose fixed-width lanes sharing leases, and rounds separated by barriers. |
| [`worklist`](crate::flow::worklist) | Drain persisted runtime-selected items and retain failed-item outcomes. |
| [`budget`](crate::flow::budget), [`events`](crate::flow::events) | Add allowance checks and lifecycle events as ordinary systems. |
| [`bind`](crate::flow::bind) | Capture immutable node policy while deriving typed system access. |
| [`TaskRunner`](crate::flow::TaskRunner), [`ScriptedRunner`](crate::flow::ScriptedRunner), [`Scenario`](crate::flow::Scenario) | Replace a single-session executor with an offline file-backed scenario. |
| [`RoleBook::load`](crate::flow::RoleBook::load), [`Role::max_micros`](crate::flow::Role::max_micros) | Merge role fragments and obtain exact role ceilings. |
| [`render`](crate::flow::render), [`stage`](crate::flow::stage), [`copy_snapshot`](crate::flow::copy_snapshot) | Render strict templates and stage isolated read-only inputs. |
| [`latest_workspace`](crate::flow::latest_workspace) | Resolve a durable step key to its latest numbered invocation folder. |
| [`SpendBook`](crate::flow::SpendBook), [`StepJournal`](crate::flow::StepJournal) | Reserve/settle money and persist typed step outputs; see their resource contracts. |
| [`RepairCounters`](crate::flow::RepairCounters), [`LeaseBook`](crate::flow::LeaseBook), [`Worklist`](crate::flow::Worklist) | Persist shared counters, work leases and sequential item results. |
| [`EventLog`](crate::flow::EventLog), [`PhaseControl`](crate::flow::PhaseControl), [`StopFlag`](crate::flow::StopFlag), [`CallDeadline`](crate::flow::CallDeadline) | Record lifecycle state, broadcast stop requests and bound call admission. |
| [`atomic_write`](crate::flow::atomic_write), [`write_json`](crate::flow::write_json), [`read_json`](crate::flow::read_json), [`append_json`](crate::flow::append_json) | Write durable records and complete JSON lines. |
| [`relative`](crate::flow::relative), [`digest`](crate::flow::digest), [`timestamp`](crate::flow::timestamp) | Validate relative identifiers, hash bytes and record UTC time. |
| [`parse_micros`](crate::flow::parse_micros), [`dollars`](crate::flow::dollars) | Convert exact decimal money at serialization boundaries. |
