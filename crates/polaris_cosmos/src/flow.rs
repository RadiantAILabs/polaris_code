//! Durable, domain-neutral graph blocks for bounded agent workflows.
//!
//! These blocks compose native Polaris graph systems, branches, scopes and loops.
//! [`repair_loop`], [`counter()`], [`budget()`] and [`events()`] are graph constructors;
//! they do not require a separate composition catalogue or plan-template layer.
//! Hosts may later adapt these builders to a catalogue without changing the
//! underlying resource and typed-output contracts.
//!
//! Keep workflow control flow in the graph: compose small blocks rather than
//! hiding a workflow or several agent calls in one system. Each [`agent`] call
//! remains an inspectable subgraph that replays, admits, reserves, stages, runs,
//! settles and collects through graph branches.
//! The existing Cosmos composition catalogue and its admission rules are
//! independent of these native builders.
//!
//! # Assembly
//!
//! A host opens the resources its graph reads, such as [`SpendBook`],
//! [`StepJournal`] and [`EventLog`], and registers them as globals, either by
//! hand or with [`FlowPlugin`]. It supplies the executor as an [`AgentRunner`]
//! around its own [`TaskRunner`]; [`testing`] holds an offline one. The
//! [flow section of the integration guide][guide] lists every graph builder and
//! the resources it reads, and the [taxonomy][taxonomy] says where the module
//! sits among the Cosmos layers.
//!
//! Every durable file lives under one run root. Only a call that ran to a stop
//! is journaled as done, and only a run started with [`RunMode::Resume`]
//! restores it; skipped, failed and interrupted calls run again.
//!
//! [guide]: https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/reference/guide.md
//! [taxonomy]: https://github.com/RadiantAILabs/polaris_code/blob/main/crates/polaris_cosmos/docs/taxonomy.md
mod agent;
mod bind;
mod budget;
mod control;
mod events;
mod io;
mod journal;
mod loops;
mod phase;
mod plugin;
mod roles;
mod staging;
pub mod testing;
mod types;
pub use agent::{agent, agent_roles};
pub use bind::{BoundSystem, bind};
pub use budget::{
    LedgerSettings, Refused, Reservation, ReserveError, SpendBook, SpendBucket, SpendSummary,
};
pub use control::{
    LaneLease, Lease, LeaseBook, RepairCounters, Verdict, WorkItem, WorkItemStatus, Worklist,
};
pub use events::{CallDeadline, EventLog, HaltReason, PhaseControl, phase_status};
#[doc(hidden)]
pub use io::digest;
pub use io::{FlowError, FlowResult, dollars, parse_micros};
pub use journal::{StepJournal, StepRecord, StepStatus, latest_workspace, memo, memo_current};
pub use loops::{CurrentItem, lease_loop, repair_loop, rounds, worklist};
pub use phase::phase;
pub use plugin::FlowPlugin;
pub use staging::FlowSettings;
#[doc(hidden)]
pub use staging::{render, stage};
pub use testing::{Scenario, ScriptEntry, ScriptedRunner};
pub use types::{
    AgentCall, AgentFailure, AgentLimits, AgentOutcome, AgentReport, AgentRequest, AgentRunner,
    Micros, OutputFile, PhasePolicy, PhaseStatus, Role, RoleBook, RunHandle, RunMode, SkipReason,
    StageSpec, StepKey, StopCause, StopFlag, TaskRunner, ToolGrant, Usage, UsdAmount,
};

/// The count a [`counter()`] block emits after it consumes one unit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CounterValue(u32);
impl CounterValue {
    /// Units consumed from the counter, including the one just taken.
    pub const fn get(self) -> u32 {
        self.0
    }
}
/// Consumes one unit from a named shared counter as a graph system.
///
/// The block emits a [`CounterValue`]. The counter must be declared in
/// [`RepairCounters`]; consuming an undeclared or exhausted counter fails the
/// system.
pub fn counter(name: &str) -> polaris::graph::Graph {
    let mut graph = polaris::graph::Graph::new();
    let name = name.to_owned();
    graph.add_system(bind::<
        polaris::system::param::Res<RepairCounters>,
        CounterValue,
        _,
    >("counter.consume", move |counters, _| {
        let name = name.clone();
        Box::pin(async move { Ok(CounterValue(counters.consume(&name)?)) })
    }));
    graph
}
/// Whether a [`budget()`] block found the phase able to grant the amount it asked about.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BudgetCheck(bool);
impl BudgetCheck {
    /// Whether the phase could grant the amount at the time of the check.
    pub const fn is_affordable(self) -> bool {
        self.0
    }
}
/// Checks whether a phase can grant a requested amount without reserving it.
///
/// The block emits a [`BudgetCheck`]. A phase with no share in the [`SpendBook`]
/// has no allowance, so the check fails for it.
pub fn budget(phase: &str, need: Micros) -> polaris::graph::Graph {
    let mut graph = polaris::graph::Graph::new();
    let phase = phase.to_owned();
    graph.add_system(
        bind::<polaris::system::param::Res<SpendBook>, BudgetCheck, _>(
            "budget.ok",
            move |book, _| {
                let phase = phase.clone();
                Box::pin(async move { Ok(BudgetCheck(book.remaining(&phase) >= need)) })
            },
        ),
    );
    graph
}
/// Emits one lifecycle event as an ordinary graph system.
///
/// The block writes `event`, the optional `phase` and `fields` to the
/// [`EventLog`], and fails the system when the line cannot be written.
pub fn events(
    event: &str,
    phase: Option<&str>,
    fields: serde_json::Value,
) -> polaris::graph::Graph {
    let mut graph = polaris::graph::Graph::new();
    let event = event.to_owned();
    let phase = phase.map(str::to_owned);
    graph.add_system(bind::<polaris::system::param::Res<EventLog>, (), _>(
        "events.emit",
        move |events, _| {
            let event = event.clone();
            let phase = phase.clone();
            let fields = fields.clone();
            Box::pin(async move {
                events.emit(&event, phase.as_deref(), fields)?;
                Ok(())
            })
        },
    ));
    graph
}
