//! Durable, domain-neutral graph blocks for bounded agent workflows.
//!
//! These blocks compose native Polaris graph systems, branches, scopes and loops.
//! [`repair_loop`], [`counter`], [`budget`] and [`events`] are graph constructors;
//! they do not require a separate composition catalogue or plan-template layer.
//! Hosts may later adapt these builders to a catalogue without changing the
//! underlying resource and typed-output contracts.
//!
//! Keep workflow control flow in the graph: compose small blocks rather than
//! hiding a workflow or several agent calls in one system. Each [`agent`] call
//! remains an inspectable reserve → stage → run → settle → collect subgraph.
//! The existing Cosmos composition catalogue and its admission rules are
//! independent of these native builders.
pub mod budget;
mod control;
pub mod events;
mod io;
mod journal;
mod roles;
mod staging;
mod types;
pub use budget::*;
pub use control::*;
pub use events::*;
pub use io::*;
pub use journal::*;
pub use staging::*;
pub use types::*;
mod agent;
pub use agent::{agent, agent_roles};
pub mod testing;
pub use testing::{Scenario, ScriptEntry, ScriptedRunner};
mod bind;
pub use bind::{BoundSystem, bind};
mod loops;
mod phase;
pub use loops::{CurrentItem, lease_loop, repair_loop, rounds, worklist};
pub use phase::phase;

/// Named counter resource and graph consumption block.
pub mod counter {
    pub use super::RepairCounters;
}
/// Consumes one unit from a named shared counter as a graph system.
pub fn counter(name: &str) -> polaris::graph::Graph {
    let mut graph = polaris::graph::Graph::new();
    let name = name.to_owned();
    graph.add_system(bind::<polaris::system::param::Res<RepairCounters>, u32, _>(
        "counter.consume",
        move |counters, _| {
            let name = name.clone();
            Box::pin(async move { Ok(counters.consume(&name)?) })
        },
    ));
    graph
}
/// Checks whether a phase can grant a requested amount without reserving it.
pub fn budget(phase: &str, need: Micros) -> polaris::graph::Graph {
    let mut graph = polaris::graph::Graph::new();
    let phase = phase.to_owned();
    graph.add_system(bind::<polaris::system::param::Res<SpendBook>, bool, _>(
        "budget.ok",
        move |book, _| {
            let phase = phase.clone();
            Box::pin(async move { Ok(book.remaining(&phase) >= need) })
        },
    ));
    graph
}
/// Emits one lifecycle event as an ordinary graph system.
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
