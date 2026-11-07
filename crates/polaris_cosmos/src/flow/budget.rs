//! Durable integer accounting with conservative recovery of abandoned calls.
use super::{Micros, StepKey, StopCause, Usage, io::*};
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
/// Granted ceiling held until a call is settled or released.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reservation {
    /// Unique reservation identity.
    pub id: String,
    /// Cumulative phase boundary.
    pub phase: String,
    /// Role identity.
    pub role: String,
    /// Maximum amount held in flight.
    pub granted_micro: Micros,
    /// Durable invocation key.
    pub step: StepKey,
    /// Relative workspace path.
    pub artifact: String,
}
/// A request below the configured minimum grant.
#[derive(Clone, Debug)]
pub struct Refused;
#[derive(Clone, Default, Serialize, Deserialize)]
struct SpendState {
    limit_micro: Micros,
    settled_micro: Micros,
    reserved: BTreeMap<String, Reservation>,
    by_phase: BTreeMap<String, Micros>,
    next: u64,
    settlements: BTreeMap<String, serde_json::Value>,
}
/// Global durable money book; use it when concurrent calls share a ceiling.
///
/// # Scope
/// Global because all branches spend from one budget.
/// # Provided By
/// Workflow boot wiring through `SpendBook::open`.
/// # Access
/// `Res<SpendBook>`; mutations serialize under an internal mutex and persist first.
/// # Alternatives
/// No accounting is appropriate only for workflows that cannot incur cost.
/// # Example
/// ```ignore
/// async fn remaining(book: Res<SpendBook>) -> u64 { book.remaining("phase") }
/// ```
#[derive(Clone)]
pub struct SpendBook {
    root: PathBuf,
    run_id: String,
    shares: BTreeMap<String, u64>,
    minimum: u64,
    inner: Arc<Mutex<SpendState>>,
}
impl GlobalResource for SpendBook {}
impl SpendBook {
    /// Opens a book; shares and minimum are fixed point fractions out of one million.
    pub fn open(
        root: &Path,
        run_id: &str,
        limit: Micros,
        shares: BTreeMap<String, u64>,
        minimum: u64,
    ) -> FlowResult<Self> {
        if minimum > 1_000_000 || shares.values().any(|v| *v > 1_000_000) {
            return Err(FlowError("invalid budget fraction".into()));
        }
        let path = root.join("metrics/spend_state.json");
        let mut state = if path.exists() {
            read_json(&path)?
        } else {
            SpendState::default()
        };
        state.limit_micro = limit;
        let book = Self {
            root: root.into(),
            run_id: run_id.into(),
            shares,
            minimum,
            inner: Arc::new(Mutex::new(state)),
        };
        book.persist(&book.inner.lock().unwrap())?;
        Ok(book)
    }
    fn persist(&self, s: &SpendState) -> FlowResult<()> {
        write_json(&self.root.join("metrics/spend_state.json"), s)?;
        // Durable settlements are authoritative. Replay only missing event ids
        // after a crash between persisting state and appending its ledger line.
        let ledger = self.root.join("metrics/cost_events.jsonl");
        let mut recorded = std::collections::BTreeSet::new();
        if ledger.exists() {
            for line in std::fs::read_to_string(&ledger)?.lines() {
                let value: serde_json::Value = serde_json::from_str(line)?;
                if let Some(id) = value["event_id"].as_str() {
                    recorded.insert(id.to_owned());
                }
            }
        } else {
            atomic_write(&ledger, b"")?;
        }
        for event in s.settlements.values() {
            if !recorded.contains(event["event_id"].as_str().expect("settlement id")) {
                append_json(&ledger, event)?;
            }
        }
        Ok(())
    }

    fn left(&self, s: &SpendState, phase: &str) -> Micros {
        let held = s.reserved.values().map(|r| r.granted_micro).sum::<u64>();
        let used = s.settled_micro.saturating_add(held);
        let cap = ((s.limit_micro as u128)
            * (self.shares.get(phase).copied().unwrap_or(1_000_000) as u128)
            / 1_000_000) as u64;
        cap.saturating_sub(used)
            .min(s.limit_micro.saturating_sub(used))
    }
    /// Cumulative phase ceiling, expressed in integer micro-dollars.
    pub fn phase_cap(&self, phase: &str) -> Micros {
        let state = self.inner.lock().unwrap();
        ((state.limit_micro as u128)
            * (self.shares.get(phase).copied().unwrap_or(1_000_000) as u128)
            / 1_000_000) as u64
    }
    /// Settled spend plus the full ceilings of all calls still in flight.
    pub fn committed(&self) -> Micros {
        let state = self.inner.lock().unwrap();
        state.settled_micro.saturating_add(
            state
                .reserved
                .values()
                .map(|r| r.granted_micro)
                .sum::<u64>(),
        )
    }
    /// Remaining cumulative allowance, counting in-flight calls at their ceiling.
    pub fn remaining(&self, phase: &str) -> Micros {
        self.left(&self.inner.lock().unwrap(), phase)
    }
    /// Reserves a role ceiling using an automatically numbered invocation.
    pub fn reserve(&self, phase: &str, role: &str, want: Micros) -> Result<Reservation, Refused> {
        self.reserve_call(phase, role, want, &StepKey::new(role), "")
    }
    /// Reserves with the invocation's durable key and workspace path.
    pub fn reserve_call(
        &self,
        phase: &str,
        role: &str,
        want: Micros,
        step: &StepKey,
        artifact: &str,
    ) -> Result<Reservation, Refused> {
        let mut state = self.inner.lock().unwrap();
        let grant = want.min(self.left(&state, phase));
        if grant == 0 || (grant as u128) * 1_000_000 < (want as u128) * (self.minimum as u128) {
            return Err(Refused);
        }
        let mut next = state.clone();
        next.next += 1;
        let id = format!("{:08}", next.next);
        let reservation = Reservation {
            id: id.clone(),
            phase: phase.into(),
            role: role.into(),
            granted_micro: grant,
            step: step.clone(),
            artifact: artifact.into(),
        };
        next.reserved.insert(id, reservation.clone());
        if self.persist(&next).is_err() {
            return Err(Refused);
        }
        *state = next;
        Ok(reservation)
    }
    /// Settles measured usage once, releasing the unused ceiling.
    pub fn settle(&self, res: &Reservation, usage: &Usage, stop: StopCause) -> FlowResult<()> {
        self.settle_status(res, usage, status(&stop))
    }
    /// Settles a failed or abandoned call with an explicit ledger status.
    pub fn settle_status(&self, res: &Reservation, usage: &Usage, status: &str) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        if !state.reserved.contains_key(&res.id) {
            return Ok(());
        }
        let mut next = state.clone();
        next.reserved.remove(&res.id);
        next.settled_micro = next
            .settled_micro
            .checked_add(usage.micros)
            .ok_or_else(|| FlowError("spend overflow".into()))?;
        *next.by_phase.entry(res.phase.clone()).or_default() += usage.micros;
        let event = json!({"schema_version":3,"event_id":digest(format!("{}:{}:{}",self.run_id,res.step,res.id).as_bytes()),"invocation_id":res.step,"timestamp":timestamp(),"agent":"polaris","provider":"amazon-bedrock","model":usage.model_id,"phase":res.phase,"stage":res.role,"status":status,"state":status,"usage":{"input_tokens":usage.input,"output_tokens":usage.output,"cached_input_tokens":usage.cache_read,"cache_creation_input_tokens":usage.cache_write},"cost_basis":"list-price-estimate","total_cost_usd":dollars(usage.micros),"artifact":res.artifact});
        next.settlements.insert(res.id.clone(), event);
        self.persist(&next)?;
        *state = next;
        Ok(())
    }
    /// Releases an unused reservation without recording a call.
    pub fn release(&self, res: Reservation) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        let mut next = state.clone();
        next.reserved.remove(&res.id);
        self.persist(&next)?;
        *state = next;
        Ok(())
    }
    /// Charges every abandoned reservation at its full granted ceiling.
    pub fn recover_abandoned(&self) -> FlowResult<Vec<Reservation>> {
        self.recover_abandoned_with_model("")
    }
    /// Charges abandoned calls with the configured model identity when known.
    pub fn recover_abandoned_with_model(&self, model_id: &str) -> FlowResult<Vec<Reservation>> {
        let abandoned = self
            .inner
            .lock()
            .unwrap()
            .reserved
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for res in &abandoned {
            self.settle_status(
                res,
                &Usage {
                    micros: res.granted_micro,
                    model_id: model_id.into(),
                    ..Usage::default()
                },
                "abandoned",
            )?;
        }
        Ok(abandoned)
    }
    /// Returns the original-compatible accounting summary.
    pub fn summary(&self) -> serde_json::Value {
        let state = self.inner.lock().unwrap();
        let mut models = BTreeMap::<String, serde_json::Value>::new();
        let mut phases = models.clone();
        let mut called = BTreeMap::<String, u64>::new();
        for line in state.settlements.values() {
            let model = line["model"].as_str().unwrap_or("");
            *called.entry(model.into()).or_default() += 1;
            let amount = parse_micros(line["total_cost_usd"].as_str().unwrap()).unwrap();
            for (book, key) in [
                (&mut models, model),
                (&mut phases, line["phase"].as_str().unwrap()),
            ] {
                let entry = book
                    .entry(key.into())
                    .or_insert(json!({"event_count":0,"micro":0}));
                entry["event_count"] = json!(entry["event_count"].as_u64().unwrap() + 1);
                entry["micro"] = json!(entry["micro"].as_u64().unwrap() + amount);
            }
        }
        for book in [&mut models, &mut phases] {
            for entry in book.values_mut() {
                entry["total_cost_usd"] = json!(dollars(entry["micro"].as_u64().unwrap()));
                entry.as_object_mut().unwrap().remove("micro");
            }
        }
        json!({"by_model":models,"by_phase":phases,"models_called":called,"total_cost_usd":dollars(state.settled_micro),"reserved_usd":dollars(state.reserved.values().map(|r|r.granted_micro).sum()),"cost_basis":"list-price-estimate"})
    }
}
/// Ledger status for a measured runner stop.
pub fn status(stop: &StopCause) -> &'static str {
    match stop {
        StopCause::Finished => "completed",
        StopCause::TurnCap => "turn_cap",
        StopCause::CostCap => "cost_cap",
        StopCause::Timeout => "timeout",
        StopCause::Interrupted => "interrupted",
        StopCause::ModelError(_) => "failed",
    }
}
