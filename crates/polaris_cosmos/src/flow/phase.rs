//! Phase enablement, lifecycle events and explicit failure policy edges.
use super::*;
use polaris::{
    graph::{ContextPolicy, Graph},
    system::param::{ErrOut, Out, Res},
};
use serde_json::json;
#[derive(Clone)]
struct PhaseEnabled(bool);
/// Journal key under which a phase records that it completed.
fn complete_key(phase: &str) -> FlowResult<StepKey> {
    StepKey::new(phase)?.child("phase.complete")
}
/// Wraps a phase graph with enablement, failure mapping and lifecycle events.
/// Checks may publish `PhaseStatus` through `PhaseControl`. Unhandled fallible
/// top-level systems take the wrapper's error edge, ending that body immediately.
///
/// The phase name is also a journal key, so it must be a valid [`StepKey`].
pub fn phase(name: &str, policy: PhasePolicy, mut body: Graph) -> Graph {
    let mut graph = Graph::new();
    let phase = name.to_owned();
    graph.add_system(bind::<(Res<PhaseControl>,Res<EventLog>,Option<Res<StepJournal>>,Option<Res<RunHandle>>),_,_>("phase.enabled",move |(control,events,journal,handle), _|{let phase=phase.clone();Box::pin(async move{
        if control.halted(){return Ok(PhaseEnabled(false));}
        if !control.enabled(&phase){control.set_status(&phase,PhaseStatus::Skipped("disabled".into()))?;events.emit("phase_end",Some(&phase),json!({"phase_name":phase,"label":phase,"status":"skipped","return_code":0,"elapsed":"0s","elapsed_seconds":0,"note":"disabled"}))?;return Ok(PhaseEnabled(false));}
        if handle.is_some_and(|handle|handle.mode==RunMode::Resume) && let Some(journal)=journal && let Some(record)=journal.read(&complete_key(&phase)?)?.filter(|r|r.status==StepStatus::Done) {
            let status:PhaseStatus=serde_json::from_value(record.output).map_err(|e|polaris::system::system::SystemError::ExecutionError(e.to_string()))?;
            control.set_status(&phase,status.clone())?;let(label,note)=phase_status(&status);events.emit("phase_end",Some(&phase),json!({"phase_name":phase,"label":phase,"status":label,"return_code":0,"elapsed":"0s","elapsed_seconds":0,"note":note,"restored":true}))?;return Ok(PhaseEnabled(false));
        }
        Ok(PhaseEnabled(true))
    })}));
    let phase = name.to_owned();
    let error_phase = name.to_owned();
    let end_phase = name.to_owned();
    body.add_error_handler(move |g| {
        g.add_system(bind::<
            (ErrOut<polaris::graph::CaughtError>, Res<PhaseControl>),
            (),
            _,
        >("phase.failure", move |(error, control), _| {
            let phase = error_phase.clone();
            Box::pin(async move {
                // Later errors usually follow from the first one, so the first failure stays.
                if !matches!(control.status(&phase), Some(PhaseStatus::Failed(_))) {
                    control.set_status(&phase, PhaseStatus::Failed(error.to_string()))?;
                }
                Ok(())
            })
        }));
    });
    graph.add_conditional_branch::<PhaseEnabled,_,_,_>(format!("phase.choose/{name}"),|v|v.0,move |g|{
        g.add_system(bind::<(Res<PhaseControl>,Res<EventLog>),PhaseStatus,_>("phase.start",move |(control,events), _|{let phase=phase.clone();Box::pin(async move{control.start(&phase);let(current,total)=control.progress(&phase);events.emit("phase_start",Some(&phase),json!({"phase_name":phase,"label":phase,"current":current,"total":total}))?;events.start_phase(&phase);Ok(PhaseStatus::Completed)})}));
        if body.is_empty(){body.add_system(||async{});}
        g.add_scope("phase.body",body,ContextPolicy::shared());
        g.add_system(bind::<(Res<PhaseControl>,Res<EventLog>,Out<PhaseStatus>,Option<Res<StepJournal>>),PhaseStatus,_>("phase.end",move |(control,events,status,journal), _|{let phase=end_phase.clone();Box::pin(async move{
            if control.status(&phase).is_none(){control.set_status(&phase,(*status).clone())?;}
            if control.expired(&phase){control.set_status(&phase,PhaseStatus::Failed("phase deadline".into()))?;}
            let(status,elapsed)=control.finish(&phase,policy)?;events.stop_phase(&phase);let(label,note)=phase_status(&status);
            if matches!(status,PhaseStatus::Completed|PhaseStatus::Degraded(_)) && let Some(journal)=journal {journal.put(&complete_key(&phase)?,serde_json::to_value(&status).map_err(|e|polaris::system::system::SystemError::ExecutionError(e.to_string()))?)?;}
            events.emit("phase_end",Some(&phase),json!({"phase_name":phase,"label":phase,"status":label,"return_code":if label=="failed"{1}else{0},"elapsed":format!("{}s",elapsed as u64),"elapsed_seconds":elapsed,"note":note}))?;Ok(status)
        })}));
    },|g|{g.add_system(||async{});});
    graph
}
