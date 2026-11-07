//! Persistent shared counters, leases and sequential work queues.
use super::{StepKey, io::*};
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
#[derive(Clone, Default, Serialize, Deserialize)]
struct CounterState {
    limits: BTreeMap<String, u32>,
    used: BTreeMap<String, u32>,
}
/// Global named counters; every edge naming a counter consumes the same cap.
#[derive(Clone)]
///
/// # Scope
/// Global because several repair edges consume the same named cap.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<RepairCounters>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RepairCounters;
/// #[system]
/// async fn inspect(value:Res<RepairCounters>)->bool { value.has_room("repair") }
/// ```
pub struct RepairCounters {
    path: PathBuf,
    inner: Arc<Mutex<CounterState>>,
}
impl GlobalResource for RepairCounters {}
impl RepairCounters {
    /// Opens named limits, preserving consumed counts.
    pub fn open(root: &Path, limits: BTreeMap<String, u32>) -> FlowResult<Self> {
        let path = root.join(".state/counters.json");
        let mut state = if path.exists() {
            read_json(&path)?
        } else {
            CounterState::default()
        };
        state.limits.extend(limits);
        write_json(&path, &state)?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(state)),
        })
    }
    /// Resets a per-item cap. Memoize the owning initialization step on restart.
    pub fn reset(&self, name: &str, limit: u32) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        let mut next = state.clone();
        next.limits.insert(name.into(), limit);
        next.used.insert(name.into(), 0);
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Whether a named cap permits another consumption.
    pub fn has_room(&self, name: &str) -> bool {
        let s = self.inner.lock().unwrap();
        s.used.get(name).copied().unwrap_or(0) < s.limits.get(name).copied().unwrap_or(0)
    }
    /// Consumes at most one unit, returning the resulting count.
    pub fn consume(&self, name: &str) -> FlowResult<u32> {
        let mut s = self.inner.lock().unwrap();
        let mut next = s.clone();
        let cap = next.limits.get(name).copied().unwrap_or(0);
        let used = next.used.entry(name.into()).or_default();
        if *used >= cap {
            return Err(FlowError(format!("counter exhausted: {name}")));
        }
        *used += 1;
        let value = *used;
        write_json(&self.path, &next)?;
        *s = next;
        Ok(value)
    }
    /// Configured limit, or zero for an undeclared name.
    pub fn limit(&self, name: &str) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .limits
            .get(name)
            .copied()
            .unwrap_or(0)
    }
    /// Already consumed units.
    pub fn used(&self, name: &str) -> u32 {
        self.inner
            .lock()
            .unwrap()
            .used
            .get(name)
            .copied()
            .unwrap_or(0)
    }
}
/// One unit of globally bounded work.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    /// Global one-based ordinal.
    pub global: u32,
    /// Worker-local one-based ordinal.
    pub local: u32,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct LeaseState {
    total: u32,
    next: u32,
    local: BTreeMap<usize, u32>,
    in_flight: BTreeMap<usize, Lease>,
}
/// Global durable allocator shared by a fixed set of graph lanes.
#[derive(Clone)]
///
/// # Scope
/// Global because all lanes share one total work cap.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<LeaseBook>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::LeaseBook;
/// #[system]
/// async fn inspect(value:Res<LeaseBook>)->u32 { value.allocated() }
/// ```
pub struct LeaseBook {
    path: PathBuf,
    inner: Arc<Mutex<LeaseState>>,
}
impl GlobalResource for LeaseBook {}
impl LeaseBook {
    /// Opens the shared cap; raising it preserves the earlier leases.
    pub fn open(root: &Path, total: u32) -> FlowResult<Self> {
        let path = root.join(".state/leases.json");
        let mut s = if path.exists() {
            read_json(&path)?
        } else {
            LeaseState::default()
        };
        if total < s.total {
            return Err(FlowError("cannot lower lease cap".into()));
        }
        s.total = total;
        write_json(&path, &s)?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(s)),
        })
    }
    /// Returns a pending lease on restart, or allocates one new cycle atomically.
    pub fn take(&self, worker: usize) -> FlowResult<Option<Lease>> {
        let mut s = self.inner.lock().unwrap();
        if let Some(lease) = s.in_flight.get(&worker) {
            return Ok(Some(lease.clone()));
        }
        if s.next >= s.total {
            return Ok(None);
        }
        let mut next = s.clone();
        next.next += 1;
        let local = next.local.entry(worker).or_default();
        *local += 1;
        let lease = Lease {
            global: next.next,
            local: *local,
        };
        next.in_flight.insert(worker, lease.clone());
        write_json(&self.path, &next)?;
        *s = next;
        Ok(Some(lease))
    }
    /// Acknowledges a finished lease, including a recorded failure.
    pub fn finish(&self, worker: usize) -> FlowResult<()> {
        let mut s = self.inner.lock().unwrap();
        let mut next = s.clone();
        next.in_flight.remove(&worker);
        write_json(&self.path, &next)?;
        *s = next;
        Ok(())
    }
    /// Total units already allocated.
    pub fn allocated(&self) -> u32 {
        self.inner.lock().unwrap().next
    }
}
/// Result of a mechanical check used by a repair loop.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Verdict {
    /// The check passed.
    Pass,
    /// The check failed with repair feedback.
    Fail(String),
}
/// Current lane lease, passed to the body as a typed output.
#[derive(Clone, Debug)]
pub struct LaneLease {
    /// Worker index.
    pub worker: usize,
    /// Granted lease, or none when the lane is drained.
    pub lease: Option<Lease>,
}
/// Durable sequential item and its completion state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable item key.
    pub key: StepKey,
    /// Domain-owned payload.
    pub value: serde_json::Value,
    /// pending, done or failed.
    pub status: String,
    /// Failure detail when present.
    pub note: String,
}
/// Global persisted sequential queue; failed items remain visible and do not block later ones.
#[derive(Clone)]
///
/// # Scope
/// Global because items and recorded outcomes belong to one durable queue.
/// # Provided By
/// Workflow boot wiring through the documented constructor or initializer.
/// # Access
/// Systems read `Res<Worklist>`; mutable books serialize updates internally.
/// # Alternatives
/// Use a local resource when branches need independent state.
/// # Example
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::Worklist;
/// #[system]
/// async fn inspect(value:Res<Worklist>)->usize { value.items().len() }
/// ```
pub struct Worklist {
    path: PathBuf,
    inner: Arc<Mutex<Vec<WorkItem>>>,
}
impl GlobalResource for Worklist {}
impl Worklist {
    /// Opens the persisted queue or initializes it exactly once.
    pub fn open(path: &Path, items: Vec<WorkItem>) -> FlowResult<Self> {
        let state = if path.exists() {
            read_json(path)?
        } else {
            items
        };
        write_json(path, &state)?;
        Ok(Self {
            path: path.into(),
            inner: Arc::new(Mutex::new(state)),
        })
    }
    /// Replaces the active queue while preserving results for matching persisted keys.
    pub fn replace(&self, items: Vec<WorkItem>) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        let next = items
            .into_iter()
            .map(|item| {
                state
                    .iter()
                    .find(|old| old.key == item.key)
                    .cloned()
                    .unwrap_or(item)
            })
            .collect::<Vec<_>>();
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Adds runtime-selected items, preserving statuses for keys already persisted.
    pub fn extend(&self, items: Vec<WorkItem>) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        let mut next = state.clone();
        for item in items {
            if !next.iter().any(|i| i.key == item.key) {
                next.push(item);
            }
        }
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Returns the first unfinished item; a restart retries the same item.
    pub fn take(&self) -> Option<WorkItem> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.status == "pending")
            .cloned()
    }
    /// Records an item result without removing its evidence.
    pub fn finish(&self, key: &StepKey, result: Result<(), String>) -> FlowResult<()> {
        let mut state = self.inner.lock().unwrap();
        let mut next = state.clone();
        let item = next
            .iter_mut()
            .find(|i| &i.key == key)
            .ok_or_else(|| FlowError(format!("unknown item: {key}")))?;
        match result {
            Ok(()) => item.status = "done".into(),
            Err(note) => {
                item.status = "failed".into();
                item.note = note;
            }
        }
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Snapshot of all item outcomes.
    pub fn items(&self) -> Vec<WorkItem> {
        self.inner.lock().unwrap().clone()
    }
}
