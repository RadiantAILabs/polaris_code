//! Durable integer accounting with conservative recovery of abandoned calls.
use super::{Micros, StepKey, StopCause, Usage, UsdAmount, io::*};
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
/// Granted ceiling held until a call is settled or released.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct Refused;
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the remaining allowance is below the minimum grant")
    }
}
impl std::error::Error for Refused {}
/// Why a reservation was not made.
///
/// Only [`Refused`](Self::Refused) means the budget is exhausted. The other
/// variants are faults, and callers should not report them as a spent budget.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReserveError {
    /// The phase allowance cannot cover the minimum grant.
    Refused(Refused),
    /// The phase has no entry in the book's shares, so it has no allowance.
    UnknownPhase(String),
    /// The step key or role name is not a valid step key.
    Invalid(FlowError),
    /// The reservation could not be saved, so nothing was held.
    Persist(FlowError),
}
impl std::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(refused) => refused.fmt(f),
            Self::UnknownPhase(phase) => write!(f, "phase {phase} has no budget share"),
            Self::Invalid(error) => write!(f, "invalid reservation: {error}"),
            Self::Persist(error) => write!(f, "reservation was not saved: {error}"),
        }
    }
}
impl std::error::Error for ReserveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(refused) => Some(refused),
            Self::UnknownPhase(_) => None,
            Self::Invalid(error) | Self::Persist(error) => Some(error),
        }
    }
}
impl From<Refused> for ReserveError {
    fn from(refused: Refused) -> Self {
        Self::Refused(refused)
    }
}
/// Fields a host stamps on every ledger line, so the ledger names the agent,
/// provider and pricing basis of the run that wrote it.
///
/// The defaults name none of them: a host that audits spend sets its own.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LedgerSettings {
    /// Version of the ledger line layout.
    pub schema_version: u32,
    /// Name of the agent that made the calls.
    pub agent: String,
    /// Provider recorded when a call's own usage names none.
    pub provider: String,
    /// How call costs were obtained, such as a list-price estimate.
    pub cost_basis: String,
}
impl Default for LedgerSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            agent: String::new(),
            provider: String::new(),
            cost_basis: "unspecified".into(),
        }
    }
}
/// One settled call, as kept in the state file and appended to the ledger.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerEvent {
    schema_version: u32,
    event_id: String,
    invocation_id: String,
    timestamp: String,
    agent: String,
    provider: String,
    model: String,
    phase: String,
    stage: String,
    status: String,
    state: String,
    usage: LedgerUsage,
    cost_basis: String,
    total_cost_usd: UsdAmount,
    artifact: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct LedgerUsage {
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    cache_creation_input_tokens: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SpendState {
    limit_micro: Micros,
    settled_micro: Micros,
    reserved: BTreeMap<String, Reservation>,
    by_phase: BTreeMap<String, Micros>,
    next: u64,
    settlements: BTreeMap<String, LedgerEvent>,
}
/// The saved state and the ids of the settlements already in the ledger file.
#[derive(Debug, Default)]
struct Book {
    state: SpendState,
    recorded: BTreeSet<String>,
}
/// Spend and call count of one model or phase.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpendBucket {
    /// Settled calls counted in this bucket.
    pub event_count: u64,
    /// Amount settled in this bucket.
    pub total_cost_usd: UsdAmount,
}
/// Settled spend, grouped by model and by phase.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SpendSummary {
    /// Settled spend per model.
    pub by_model: BTreeMap<String, SpendBucket>,
    /// Settled spend per phase.
    pub by_phase: BTreeMap<String, SpendBucket>,
    /// Number of settled calls per model.
    pub models_called: BTreeMap<String, u64>,
    /// Total settled spend.
    pub total_cost_usd: UsdAmount,
    /// Ceilings of the calls still in flight.
    pub reserved_usd: UsdAmount,
    /// How the costs were obtained, from the ledger settings.
    pub cost_basis: String,
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
#[derive(Clone, Debug)]
pub struct SpendBook {
    root: PathBuf,
    run_id: String,
    shares: BTreeMap<String, u64>,
    minimum: u64,
    ledger: LedgerSettings,
    inner: Arc<Mutex<Book>>,
}
impl GlobalResource for SpendBook {}
impl SpendBook {
    /// Opens a book; shares and minimum are fixed point fractions out of one million.
    ///
    /// Only phases named in `shares` have an allowance. Reopening a saved book
    /// keeps its spend and may raise the limit but never lower it.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] for a fraction above one million or a
    /// `limit` below the one already saved, and a [`FlowError`] when the saved
    /// state or ledger cannot be read or written.
    pub fn open(
        root: &Path,
        run_id: &str,
        limit: Micros,
        shares: BTreeMap<String, u64>,
        minimum: u64,
    ) -> FlowResult<Self> {
        if minimum > 1_000_000 || shares.values().any(|v| *v > 1_000_000) {
            return Err(FlowError::invalid("invalid budget fraction"));
        }
        let path = root.join("metrics/spend_state.json");
        let mut state: SpendState = if path.exists() {
            read_json(&path)?
        } else {
            SpendState::default()
        };
        if limit < state.limit_micro {
            return Err(FlowError::invalid(format!(
                "cannot lower the spend limit from {} to {}",
                dollars(state.limit_micro),
                dollars(limit)
            )));
        }
        state.limit_micro = limit;
        let recorded = load_recorded(&root.join("metrics/cost_events.jsonl"))?;
        let book = Self {
            root: root.into(),
            run_id: run_id.into(),
            shares,
            minimum,
            ledger: LedgerSettings::default(),
            inner: Arc::new(Mutex::new(Book { state, recorded })),
        };
        {
            let mut guard = book.inner.guard();
            let Book { state, recorded } = &mut *guard;
            book.persist(state, recorded)?;
        }
        Ok(book)
    }
    /// Stamps the agent, provider and cost basis of `ledger` on every line this
    /// book writes from now on.
    pub fn with_ledger(mut self, ledger: LedgerSettings) -> Self {
        self.ledger = ledger;
        self
    }
    /// Saves the state, then appends the ledger line of every settlement that
    /// is not in the ledger yet.
    ///
    /// Durable settlements are authoritative, so a crash between the two writes
    /// is repaired by the next save. Ids already in the ledger are kept in
    /// memory, so a save never re-reads the ledger file.
    fn persist(&self, state: &SpendState, recorded: &mut BTreeSet<String>) -> FlowResult<()> {
        write_json(&self.root.join("metrics/spend_state.json"), state)?;
        let ledger = self.root.join("metrics/cost_events.jsonl");
        for event in state.settlements.values() {
            if !recorded.contains(&event.event_id) {
                append_json(&ledger, event)?;
                recorded.insert(event.event_id.clone());
            }
        }
        Ok(())
    }
    fn cap(&self, limit: Micros, phase: &str) -> Micros {
        let share = self.shares.get(phase).copied().unwrap_or(0);
        ((limit as u128) * (share as u128) / 1_000_000) as u64
    }
    fn left(&self, s: &SpendState, phase: &str) -> Micros {
        let used = s.settled_micro.saturating_add(held(s));
        self.cap(s.limit_micro, phase)
            .saturating_sub(used)
            .min(s.limit_micro.saturating_sub(used))
    }
    /// Cumulative phase ceiling, expressed in integer micro-dollars. A phase
    /// without a share has a ceiling of zero.
    pub fn phase_cap(&self, phase: &str) -> Micros {
        self.cap(self.inner.guard().state.limit_micro, phase)
    }
    /// Settled spend plus the full ceilings of all calls still in flight.
    pub fn committed(&self) -> Micros {
        let guard = self.inner.guard();
        guard.state.settled_micro.saturating_add(held(&guard.state))
    }
    /// Remaining cumulative allowance, counting in-flight calls at their ceiling.
    /// A phase without a share has none.
    pub fn remaining(&self, phase: &str) -> Micros {
        self.left(&self.inner.guard().state, phase)
    }
    /// Reserves a role ceiling using an automatically numbered invocation.
    ///
    /// # Errors
    ///
    /// See [`reserve_call`](Self::reserve_call). A role name that is not a
    /// valid step key fails with [`ReserveError::Invalid`].
    pub fn reserve(
        &self,
        phase: &str,
        role: &str,
        want: Micros,
    ) -> Result<Reservation, ReserveError> {
        let step = StepKey::new(role).map_err(ReserveError::Invalid)?;
        self.reserve_call(phase, role, want, &step, "")
    }
    /// Reserves with the invocation's durable key and workspace path.
    ///
    /// # Errors
    ///
    /// Returns [`ReserveError::Refused`] when the phase allowance cannot cover
    /// the minimum grant, [`ReserveError::UnknownPhase`] when the phase has no
    /// share, and [`ReserveError::Persist`] when the reservation cannot be
    /// saved. In every case nothing is held.
    pub fn reserve_call(
        &self,
        phase: &str,
        role: &str,
        want: Micros,
        step: &StepKey,
        artifact: &str,
    ) -> Result<Reservation, ReserveError> {
        if !self.shares.contains_key(phase) {
            return Err(ReserveError::UnknownPhase(phase.into()));
        }
        let mut guard = self.inner.guard();
        let book = &mut *guard;
        let grant = want.min(self.left(&book.state, phase));
        if grant == 0 || (grant as u128) * 1_000_000 < (want as u128) * (self.minimum as u128) {
            return Err(Refused.into());
        }
        // Every change is made on a copy and published once it is saved. The
        // save rewrites the whole state file, so the copy costs no more than
        // the write it guards.
        let mut next = book.state.clone();
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
        self.persist(&next, &mut book.recorded)
            .map_err(ReserveError::Persist)?;
        book.state = next;
        Ok(reservation)
    }
    /// Settles measured usage once, releasing the unused ceiling.
    ///
    /// # Errors
    ///
    /// See [`settle_status`](Self::settle_status).
    pub fn settle(&self, res: &Reservation, usage: &Usage, stop: StopCause) -> FlowResult<()> {
        self.settle_status(res, usage, stop.ledger_status())
    }
    /// Settles a failed or abandoned call with an explicit ledger status.
    ///
    /// A reservation that is already settled is left alone.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the spend overflows or the settlement
    /// cannot be saved.
    pub fn settle_status(&self, res: &Reservation, usage: &Usage, status: &str) -> FlowResult<()> {
        let mut guard = self.inner.guard();
        let book = &mut *guard;
        if !book.state.reserved.contains_key(&res.id) {
            return Ok(());
        }
        let mut next = book.state.clone();
        next.reserved.remove(&res.id);
        next.settled_micro = next
            .settled_micro
            .checked_add(usage.micros)
            .ok_or_else(|| FlowError::invalid("spend overflow"))?;
        *next.by_phase.entry(res.phase.clone()).or_default() += usage.micros;
        let provider = if usage.provider.is_empty() {
            self.ledger.provider.clone()
        } else {
            usage.provider.clone()
        };
        let event = LedgerEvent {
            schema_version: self.ledger.schema_version,
            event_id: digest(format!("{}:{}:{}", self.run_id, res.step, res.id).as_bytes()),
            invocation_id: res.step.to_string(),
            timestamp: timestamp(),
            agent: self.ledger.agent.clone(),
            provider,
            model: usage.model_id.clone(),
            phase: res.phase.clone(),
            stage: res.role.clone(),
            status: status.into(),
            state: status.into(),
            usage: LedgerUsage {
                input_tokens: usage.input,
                output_tokens: usage.output,
                cached_input_tokens: usage.cache_read,
                cache_creation_input_tokens: usage.cache_write,
            },
            cost_basis: self.ledger.cost_basis.clone(),
            total_cost_usd: UsdAmount::from_micros(usage.micros),
            artifact: res.artifact.clone(),
        };
        next.settlements.insert(res.id.clone(), event);
        self.persist(&next, &mut book.recorded)?;
        book.state = next;
        Ok(())
    }
    /// Releases an unused reservation without recording a call.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the release cannot be saved; the
    /// reservation then stays held.
    pub fn release(&self, res: Reservation) -> FlowResult<()> {
        let mut guard = self.inner.guard();
        let book = &mut *guard;
        let mut next = book.state.clone();
        next.reserved.remove(&res.id);
        self.persist(&next, &mut book.recorded)?;
        book.state = next;
        Ok(())
    }
    /// Charges every abandoned reservation at its full granted ceiling.
    ///
    /// # Errors
    ///
    /// See [`settle_status`](Self::settle_status).
    pub fn recover_abandoned(&self) -> FlowResult<Vec<Reservation>> {
        self.recover_abandoned_with_model("")
    }
    /// Charges abandoned calls with the configured model identity when known.
    ///
    /// # Errors
    ///
    /// See [`settle_status`](Self::settle_status).
    pub fn recover_abandoned_with_model(&self, model_id: &str) -> FlowResult<Vec<Reservation>> {
        let abandoned = self
            .inner
            .guard()
            .state
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
    /// Summarizes settled spend by model and by phase.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when a total overflows.
    pub fn summary(&self) -> FlowResult<SpendSummary> {
        let guard = self.inner.guard();
        let state = &guard.state;
        let mut summary = SpendSummary {
            total_cost_usd: UsdAmount::from_micros(state.settled_micro),
            reserved_usd: UsdAmount::from_micros(held(state)),
            cost_basis: self.ledger.cost_basis.clone(),
            ..SpendSummary::default()
        };
        for event in state.settlements.values() {
            *summary
                .models_called
                .entry(event.model.clone())
                .or_default() += 1;
            add_to_bucket(&mut summary.by_model, &event.model, event.total_cost_usd)?;
            add_to_bucket(&mut summary.by_phase, &event.phase, event.total_cost_usd)?;
        }
        Ok(summary)
    }
}
/// Sum of the ceilings of every call still in flight.
fn held(state: &SpendState) -> Micros {
    state
        .reserved
        .values()
        .fold(0, |sum, r| sum.saturating_add(r.granted_micro))
}
fn add_to_bucket(
    buckets: &mut BTreeMap<String, SpendBucket>,
    key: &str,
    amount: UsdAmount,
) -> FlowResult<()> {
    let bucket = buckets.entry(key.into()).or_default();
    bucket.event_count += 1;
    bucket.total_cost_usd = bucket
        .total_cost_usd
        .micros()
        .checked_add(amount.micros())
        .map(UsdAmount::from_micros)
        .ok_or_else(|| FlowError::invalid("spend overflow"))?;
    Ok(())
}
/// Reads the settlement ids already in the ledger file, creating an empty file
/// when there is none.
///
/// A crash can leave a torn last line. The state file still holds that
/// settlement and replays it, so the fragment is cut off before new lines are
/// appended behind it. An unreadable line anywhere else is corruption.
fn load_recorded(path: &Path) -> FlowResult<BTreeSet<String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            atomic_write(path, b"")?;
            return Ok(BTreeSet::new());
        }
        Err(error) => return Err(error.into()),
    };
    let mut recorded = BTreeSet::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let body = line.trim();
        if body.is_empty() {
            continue;
        }
        let event = line
            .ends_with('\n')
            .then(|| serde_json::from_str::<serde_json::Value>(body).ok())
            .flatten();
        match event {
            Some(event) => {
                if let Some(id) = event["event_id"].as_str() {
                    recorded.insert(id.to_owned());
                }
            }
            None if offset == text.len() => {
                let file = OpenOptions::new().write(true).open(path)?;
                file.set_len(start as u64)?;
                file.sync_all()?;
                break;
            }
            None => {
                return Err(FlowError::invalid(format!(
                    "corrupt ledger line at byte {start} of {}",
                    path.display()
                )));
            }
        }
    }
    Ok(recorded)
}
