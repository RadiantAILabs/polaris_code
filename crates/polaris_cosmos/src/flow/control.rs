//! Persistent shared counters, leases and sequential work queues.
use super::{StepKey, io::*};
use polaris::system::resource::GlobalResource;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CounterState {
    limits: BTreeMap<String, u32>,
    used: BTreeMap<String, u32>,
}
/// Named counters that survive a restart; use `Res<RepairCounters>` when
/// several check-and-repair edges must spend one shared attempt cap.
///
/// Every edge that names a counter consumes the same cap, so a repair cannot be
/// retried past it by taking another path through the graph.
///
/// # Scope
///
/// **Global.** Several repair edges, possibly in parallel branches, consume the
/// same named cap.
///
/// # Provided by
///
/// Consumer-supplied. The host builds it with [`RepairCounters::open`] and
/// registers it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`counter()`](fn@super::counter) block and [`repair_loop`](super::repair_loop)
/// read it.
///
/// # Access
///
/// `Res<RepairCounters>` only. The value is not immutable: `consume` and
/// `reset` change counts through `&self`. A clone shares one
/// `Arc<Mutex<..>>` of the counts, so every clone sees every change, and a
/// change is saved to the file before the in-memory counts take it.
///
/// # Alternatives
///
/// [`SpendBook`](super::SpendBook) bounds money and [`LeaseBook`] bounds
/// the total units of work shared by a fixed set of lanes. Use an ordinary local
/// resource instead when branches need independent counts that do not have to
/// survive a restart.
///
/// # Serialization
///
/// Saved as JSON in `.state/counters.json` under the run root, holding the
/// `limits` and the `used` count of each name. [`open`](Self::open) reads that
/// file when it exists, keeps the consumed counts, and takes the limits the host
/// passes in over the saved ones. Each `reset` and `consume` rewrites the file,
/// so a resumed run continues with the units already used.
///
/// # Example system
///
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::RepairCounters;
/// #[system]
/// async fn inspect(value:Res<RepairCounters>)->bool { value.has_room("repair").unwrap_or(false) }
/// ```
#[derive(Clone, Debug)]
pub struct RepairCounters {
    path: PathBuf,
    inner: Arc<Mutex<CounterState>>,
}
impl GlobalResource for RepairCounters {}
impl RepairCounters {
    /// Opens named limits, preserving consumed counts.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the saved counters cannot be read or written.
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
    /// Resets a per-item cap, declaring the counter when it is new. Memoize the
    /// owning initialization step on restart.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the counters cannot be saved.
    pub fn reset(&self, name: &str, limit: u32) -> FlowResult<()> {
        let mut state = self.inner.guard();
        // Copy, save, then publish: memory never runs ahead of the saved state.
        let mut next = state.clone();
        next.limits.insert(name.into(), limit);
        next.used.insert(name.into(), 0);
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Whether a named cap permits another consumption.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when the counter was never declared.
    pub fn has_room(&self, name: &str) -> FlowResult<bool> {
        let s = self.inner.guard();
        let limit = s.limits.get(name).ok_or_else(|| undeclared(name))?;
        Ok(s.used.get(name).copied().unwrap_or(0) < *limit)
    }
    /// Consumes at most one unit, returning the resulting count.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when the counter was never declared or
    /// is exhausted, and a [`FlowError`] when the counters cannot be saved.
    pub fn consume(&self, name: &str) -> FlowResult<u32> {
        let mut s = self.inner.guard();
        let mut next = s.clone();
        let cap = *next.limits.get(name).ok_or_else(|| undeclared(name))?;
        let used = next.used.entry(name.into()).or_default();
        if *used >= cap {
            return Err(FlowError::invalid(format!("counter exhausted: {name}")));
        }
        *used += 1;
        let value = *used;
        write_json(&self.path, &next)?;
        *s = next;
        Ok(value)
    }
    /// Configured limit, or none for an undeclared name.
    pub fn limit(&self, name: &str) -> Option<u32> {
        self.inner.guard().limits.get(name).copied()
    }
    /// Already consumed units.
    pub fn used(&self, name: &str) -> u32 {
        self.inner.guard().used.get(name).copied().unwrap_or(0)
    }
}
fn undeclared(name: &str) -> FlowError {
    FlowError::invalid(format!("undeclared counter: {name}"))
}
/// One unit of globally bounded work.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub struct Lease {
    /// Global one-based ordinal.
    pub global: u32,
    /// Worker-local one-based ordinal.
    pub local: u32,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct LeaseState {
    total: u32,
    next: u32,
    local: BTreeMap<usize, u32>,
    in_flight: BTreeMap<usize, Lease>,
}
/// Durable allocator of numbered work units; use `Res<LeaseBook>` when a fixed
/// set of parallel lanes must share one total cap and each unit must be handed
/// out once, even across a restart.
///
/// Each lane takes a [`Lease`] carrying a global and a lane-local ordinal. A
/// lease stays in flight until the lane finishes it, so a restart gives the same
/// lane the same lease again.
///
/// # Scope
///
/// **Global.** All lanes draw from one total work cap.
///
/// # Provided by
///
/// Consumer-supplied. The host builds it with [`LeaseBook::open`] and registers
/// it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`lease_loop`](super::lease_loop) block reads it.
///
/// # Access
///
/// `Res<LeaseBook>` only. The value is not immutable: `take` and `finish`
/// change the allocation through `&self`. A clone shares one `Arc<Mutex<..>>`,
/// and a change is saved to the file before the in-memory state takes it.
///
/// # Alternatives
///
/// [`RepairCounters`] caps named attempts that any edge may consume, and
/// [`Worklist`] hands out items chosen at run time rather than numbered units.
/// Use an ordinary local resource when each lane needs its own independent
/// budget.
///
/// # Serialization
///
/// Saved as JSON in `.state/leases.json` under the run root: the total, the next
/// ordinal, each lane's local count and the lease each lane holds in flight.
/// [`open`](Self::open) reads it when it exists. It may raise the total and
/// keeps the earlier leases, and it refuses to lower the total.
///
/// # Example system
///
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::LeaseBook;
/// #[system]
/// async fn inspect(value:Res<LeaseBook>)->u32 { value.allocated() }
/// ```
#[derive(Clone, Debug)]
pub struct LeaseBook {
    path: PathBuf,
    inner: Arc<Mutex<LeaseState>>,
}
impl GlobalResource for LeaseBook {}
impl LeaseBook {
    /// Opens the shared cap; raising it preserves the earlier leases.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] when `total` is below the cap already
    /// saved, and a [`FlowError`] when the saved leases cannot be read or written.
    pub fn open(root: &Path, total: u32) -> FlowResult<Self> {
        let path = root.join(".state/leases.json");
        let mut s = if path.exists() {
            read_json(&path)?
        } else {
            LeaseState::default()
        };
        if total < s.total {
            return Err(FlowError::invalid("cannot lower lease cap"));
        }
        s.total = total;
        write_json(&path, &s)?;
        Ok(Self {
            path,
            inner: Arc::new(Mutex::new(s)),
        })
    }
    /// Returns a pending lease on restart, or allocates one new cycle atomically.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the leases cannot be saved.
    pub fn take(&self, worker: usize) -> FlowResult<Option<Lease>> {
        let mut s = self.inner.guard();
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
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the leases cannot be saved.
    pub fn finish(&self, worker: usize) -> FlowResult<()> {
        let mut s = self.inner.guard();
        let mut next = s.clone();
        next.in_flight.remove(&worker);
        write_json(&self.path, &next)?;
        *s = next;
        Ok(())
    }
    /// Total units already allocated.
    pub fn allocated(&self) -> u32 {
        self.inner.guard().next
    }
}
/// Result of a mechanical check used by a repair loop.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[non_exhaustive]
pub enum Verdict {
    /// The check passed.
    Pass,
    /// The check failed with repair feedback.
    Fail(String),
}
/// Current lane lease, passed to the body as a typed output.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct LaneLease {
    /// Worker index.
    pub worker: usize,
    /// Granted lease, or none when the lane is drained.
    pub lease: Option<Lease>,
}
/// Where a work item stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum WorkItemStatus {
    /// Not finished; the next take returns it, and a restart retries it.
    Pending,
    /// Finished successfully.
    Done,
    /// Finished with a recorded failure; later items still run.
    Failed,
}
/// Durable sequential item and its completion state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable item key.
    pub key: StepKey,
    /// Domain-owned payload.
    pub value: serde_json::Value,
    /// Where the item stands. [`Worklist::take`] returns only `Pending` items,
    /// and a restart returns a pending item again.
    pub status: WorkItemStatus,
    /// Failure detail, recorded when the item finishes as `Failed`.
    pub note: String,
}
/// Persisted sequential queue; use `Res<Worklist>` when a workflow drains a list
/// of items chosen at run time and must keep each item's outcome across a
/// restart.
///
/// A failed item stays in the queue with its note and does not block the items
/// after it.
///
/// # Scope
///
/// **Global.** The items and their recorded outcomes belong to one durable
/// queue.
///
/// # Provided by
///
/// Consumer-supplied. The host builds it with [`Worklist::open`] and registers
/// it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`worklist`](super::worklist) block reads it.
///
/// # Access
///
/// `Res<Worklist>` only. The value is not immutable: `replace`, `extend` and
/// `finish` change the queue through `&self`. A clone shares one
/// `Arc<Mutex<..>>`, and a change is saved to the file before the in-memory
/// queue takes it.
///
/// # Alternatives
///
/// [`LeaseBook`] allocates numbered units across parallel lanes instead of
/// named items in order. Use an ordinary local resource when the items need not
/// survive a restart.
///
/// # Serialization
///
/// Saved as a JSON array of [`WorkItem`] at the path given to
/// [`open`](Self::open). `open` uses the items passed in only when that file does
/// not exist yet, so the queue is initialized once; afterwards it restores the
/// saved items with their statuses and notes. Each change rewrites the file.
///
/// # Example system
///
/// ```
/// use polaris::system::{system,param::Res};
/// use polaris_cosmos::flow::Worklist;
/// #[system]
/// async fn inspect(value:Res<Worklist>)->usize { value.items().len() }
/// ```
#[derive(Clone, Debug)]
pub struct Worklist {
    path: PathBuf,
    inner: Arc<Mutex<Vec<WorkItem>>>,
}
impl GlobalResource for Worklist {}
impl Worklist {
    /// Opens the persisted queue or initializes it exactly once.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the saved queue cannot be read or written.
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
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the queue cannot be saved.
    pub fn replace(&self, items: Vec<WorkItem>) -> FlowResult<()> {
        let mut state = self.inner.guard();
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
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the queue cannot be saved.
    pub fn extend(&self, items: Vec<WorkItem>) -> FlowResult<()> {
        let mut state = self.inner.guard();
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
            .guard()
            .iter()
            .find(|i| i.status == WorkItemStatus::Pending)
            .cloned()
    }
    /// Records an item result without removing its evidence.
    ///
    /// # Errors
    ///
    /// Returns [`FlowError::Invalid`] for an unknown key, and a [`FlowError`]
    /// when the queue cannot be saved.
    pub fn finish(&self, key: &StepKey, result: Result<(), String>) -> FlowResult<()> {
        let mut state = self.inner.guard();
        let mut next = state.clone();
        let item = next
            .iter_mut()
            .find(|i| &i.key == key)
            .ok_or_else(|| FlowError::invalid(format!("unknown item: {key}")))?;
        match result {
            Ok(()) => item.status = WorkItemStatus::Done,
            Err(note) => {
                item.status = WorkItemStatus::Failed;
                item.note = note;
            }
        }
        write_json(&self.path, &next)?;
        *state = next;
        Ok(())
    }
    /// Snapshot of all item outcomes.
    pub fn items(&self) -> Vec<WorkItem> {
        self.inner.guard().clone()
    }
}
