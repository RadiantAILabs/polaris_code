//! Completed-step records and reusable graph memoization.
use super::{StepKey, io::*};
use polaris::{
    graph::Graph,
    system::{
        param::{Out, Res},
        resource::GlobalResource,
        system::SystemError,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
/// Where a step stands in the journal. Only [`Done`](Self::Done) records are
/// restored by a resumed run; every other state runs the step again.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StepStatus {
    /// The step began and has not finished.
    Started,
    /// The step finished and its output is stored.
    Done,
    /// The step failed or was interrupted.
    Failed,
    /// The step was skipped without running.
    Skipped,
}
/// A durable step output; only `done` records are restored.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct StepRecord {
    /// Stable step identity.
    pub key: StepKey,
    /// Completion status.
    pub status: StepStatus,
    /// UTC start time.
    pub started: String,
    /// UTC completion time.
    pub ended: String,
    /// Serialized typed output.
    pub output: serde_json::Value,
}
/// Durable record of finished steps; use `Res<StepJournal>` when a system must
/// reuse the output of a step that already finished, so a restarted workflow
/// does not repeat it.
///
/// [`memo`] and [`memo_current`] build on it: they restore a finished step's
/// typed output and record a new one. It also hands out the numbered workspace
/// folder of each pass over a step.
///
/// # Scope
///
/// **Global.** Parallel branches share one set of durable step keys.
///
/// # Provided by
///
/// Consumer-supplied. The host builds it with [`StepJournal::open`] and
/// registers it as a global, directly or through
/// [`FlowPlugin::with_resource`](super::FlowPlugin::with_resource). The
/// [`agent`](fn@super::agent), [`memo`] and [`memo_current`] blocks read it.
///
/// # Access
///
/// `Res<StepJournal>` only. The value holds no step state in memory, so a clone
/// is as good as the original. `put`, `edge`, `workspace` and `abandon` change
/// files through `&self` and take one shared `Arc<Mutex<()>>` while they do, so
/// parallel branches never interleave their writes.
///
/// # Alternatives
///
/// A system that is not memoized runs every time, which is right for work that
/// is cheap or must repeat. [`SpendBook`](super::SpendBook) is the resource
/// for money rather than for step output.
///
/// # Serialization
///
/// Everything is kept under the run root:
///
/// - `.state/steps/<key>.json` holds the [`StepRecord`] of each step: its
///   [`StepStatus`], times and typed output as JSON.
/// - `journal.jsonl` gets one line per step edge, with the step's output digest
///   when it finished.
/// - `.state/work_ordinals.json` holds the last workspace number of each step,
///   and the workspaces themselves are `work/<key>/<NNN>`.
///
/// [`open`](Self::open) reads nothing back. [`get`](Self::get) restores only a
/// record whose status is `Done`; a step that started, failed or was skipped
/// runs again.
///
/// # Example system
///
/// ```
/// use polaris::system::{param::Res, system, system::SystemError};
/// use polaris_cosmos::flow::{StepJournal, StepKey};
///
/// /// Reuses the saved plan after a restart, or records it the first time.
/// #[system]
/// async fn plan_once(journal: Res<StepJournal>) -> Result<u64, SystemError> {
///     let key = StepKey::new("plan")?;
///     if let Some(done) = journal.get(&key) {
///         return Ok(done.output.as_u64().unwrap_or_default());
///     }
///     journal.put(&key, serde_json::json!(7))?;
///     Ok(7)
/// }
/// ```
#[derive(Clone, Debug)]
pub struct StepJournal {
    root: PathBuf,
    lock: Arc<Mutex<()>>,
}
impl GlobalResource for StepJournal {}
impl StepJournal {
    /// Opens a journal rooted in a workflow directory.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the journal directory cannot be created.
    pub fn open(root: &Path) -> FlowResult<Self> {
        std::fs::create_dir_all(root.join(".state/steps"))?;
        Ok(Self {
            root: root.into(),
            lock: Arc::default(),
        })
    }
    fn path(&self, key: &StepKey) -> FlowResult<PathBuf> {
        Ok(self
            .root
            .join(".state/steps")
            .join(format!("{}.json", relative(key.as_str())?.display())))
    }
    /// Restores only a durably completed output.
    pub fn get(&self, key: &StepKey) -> Option<StepRecord> {
        self.read(key)
            .ok()
            .flatten()
            .filter(|r| r.status == StepStatus::Done)
    }
    /// Reads a record, distinguishing corruption from absence.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the record exists but cannot be read.
    pub fn read(&self, key: &StepKey) -> FlowResult<Option<StepRecord>> {
        let path = self.path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }
    /// Publishes a completed output and appends its digest to the journal.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the record cannot be written.
    pub fn put(&self, key: &StepKey, output: serde_json::Value) -> FlowResult<()> {
        let _lock = self.lock.guard();
        let now = timestamp();
        let record = StepRecord {
            key: key.clone(),
            status: StepStatus::Done,
            started: self
                .read(key)?
                .map_or_else(|| now.clone(), |record| record.started),
            ended: now.clone(),
            output,
        };
        write_json(&self.path(key)?, &record)?;
        append_json(
            &self.root.join("journal.jsonl"),
            &serde_json::json!({"ts":now,"key":key,"state":StepStatus::Done,"digest":digest(&serde_json::to_vec(&record.output)?)}),
        )
    }
    /// Appends a non-completion step edge.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the record cannot be written, and
    /// [`FlowError::Invalid`] for [`StepStatus::Done`], which only
    /// [`put`](Self::put) records.
    pub fn edge(&self, key: &StepKey, state: StepStatus) -> FlowResult<()> {
        if state == StepStatus::Done {
            return Err(FlowError::invalid("a done step is recorded with put"));
        }
        let _lock = self.lock.guard();
        let now = timestamp();
        let started = if state == StepStatus::Started {
            now.clone()
        } else {
            self.read(key)?
                .map_or_else(|| now.clone(), |record| record.started)
        };
        write_json(
            &self.path(key)?,
            &StepRecord {
                key: key.clone(),
                status: state,
                started,
                ended: if state == StepStatus::Started {
                    String::new()
                } else {
                    now.clone()
                },
                output: serde_json::Value::Null,
            },
        )?;
        append_json(
            &self.root.join("journal.jsonl"),
            &serde_json::json!({"ts":timestamp(),"key":key,"state":state,"digest":null}),
        )
    }
    /// Allocates a fresh numbered workspace on every pass over a step.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when the workspace cannot be created.
    pub fn workspace(&self, key: &StepKey) -> FlowResult<PathBuf> {
        let _lock = self.lock.guard();
        let parent = self.root.join("work").join(relative(key.as_str())?);
        std::fs::create_dir_all(&parent)?;
        let state_path = self.root.join(".state/work_ordinals.json");
        let mut ordinals: std::collections::BTreeMap<String, u64> = if state_path.exists() {
            read_json(&state_path)?
        } else {
            std::collections::BTreeMap::new()
        };
        let previous = ordinals.get(key.as_str()).copied().unwrap_or(0);
        let mut ordinal = previous + 1;
        loop {
            let path = parent.join(format!("{ordinal:03}"));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    ordinals.insert(key.to_string(), ordinal);
                    write_json(&state_path, &ordinals)?;
                    write_json(
                        &path.join(".invocation.json"),
                        &serde_json::json!({"step":key}),
                    )?;
                    return Ok(path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => ordinal += 1,
                Err(e) => return Err(e.into()),
            }
        }
    }
    /// Moves a half-finished workspace into a durable abandoned directory.
    ///
    /// # Errors
    ///
    /// Returns a [`FlowError`] when `path` is outside the work root or cannot
    /// be moved.
    pub fn abandon(&self, path: &Path) -> FlowResult<()> {
        let _lock = self.lock.guard();
        if !path.exists() {
            return Ok(());
        }
        let relative = path
            .strip_prefix(self.root.join("work"))
            .map_err(|_| FlowError::invalid("abandoned folder outside work root"))?;
        let dest = self.root.join("work/_abandoned").join(relative);
        if let Some(p) = dest.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::rename(path, dest)?;
        Ok(())
    }
}
#[derive(Clone)]
struct MemoProbe {
    key: StepKey,
    record: Option<StepRecord>,
}
/// Wraps a typed output-producing subgraph in a durable completion branch.
///
/// Use a distinct key for each loop iteration. The body is ordinary graph wiring.
/// A key that was already completed returns its stored output on every later
/// pass, whatever the [`RunMode`](super::RunMode), so a key reused across
/// iterations repeats the first result instead of running again.
///
/// # Panics
///
/// Panics when `body` is not a well-formed graph: every node must be reachable
/// from its entry. An empty body is accepted.
pub fn memo<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static>(
    key: StepKey,
    body: Graph,
) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(super::bind::<Res<StepJournal>, _, _>(
        "memo.probe",
        move |journal, _| {
            let key = key.clone();
            Box::pin(async move {
                let record = journal.read(&key)?.filter(|r| r.status == StepStatus::Done);
                if record.is_none() {
                    journal.edge(&key, StepStatus::Started)?;
                }
                Ok(MemoProbe { key, record })
            })
        },
    ));
    memo_branch::<T>(graph, body)
}
/// Memoizes a body using the preceding system's runtime `Out<StepKey>`.
/// This keeps per-item and per-iteration identities in graph data rather than shape.
/// A key that was already completed returns its stored output, as for [`memo`].
///
/// # Panics
///
/// Panics when `body` is not a well-formed graph: every node must be reachable
/// from its entry. An empty body is accepted.
pub fn memo_current<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static>(
    body: Graph,
) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(super::bind::<(Out<StepKey>, Res<StepJournal>), _, _>(
        "memo.current",
        |(key, journal), _| {
            Box::pin(async move {
                let key = (*key).clone();
                let record = journal.read(&key)?.filter(|r| r.status == StepStatus::Done);
                if record.is_none() {
                    journal.edge(&key, StepStatus::Started)?;
                }
                Ok(MemoProbe { key, record })
            })
        },
    ));
    memo_branch::<T>(graph, body)
}
fn memo_branch<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static>(
    mut graph: Graph,
    body: Graph,
) -> Graph {
    graph.add_conditional_branch::<MemoProbe, _, _, _>(
        "memo.hit",
        |p| p.record.is_some(),
        |g| {
            g.add_system(super::bind::<Out<MemoProbe>, T, _>(
                "memo.restore",
                |probe, _| {
                    Box::pin(async move {
                        let record = probe.record.as_ref().ok_or_else(|| {
                            SystemError::ExecutionError("memo restore without a record".into())
                        })?;
                        serde_json::from_value(record.output.clone())
                            .map_err(|e| SystemError::ExecutionError(e.to_string()))
                    })
                },
            ));
        },
        |g| {
            g.append(body).expect("memo body must be a connected graph");
            g.add_system(super::bind::<
                (
                    Out<T>,
                    Out<MemoProbe>,
                    Res<StepJournal>,
                    Option<Res<super::PhaseControl>>,
                ),
                T,
                _,
            >(
                "memo.record",
                |(value, probe, journal, control), _| {
                    Box::pin(async move {
                        if control.is_some_and(|control| control.interrupted()) {
                            journal.edge(&probe.key, StepStatus::Failed)?;
                            return Ok((*value).clone());
                        }
                        journal.put(
                            &probe.key,
                            serde_json::to_value(&*value)
                                .map_err(|e| SystemError::ExecutionError(e.to_string()))?,
                        )?;
                        Ok((*value).clone())
                    })
                },
            ));
        },
    );
    graph
}

/// Finds the latest numbered invocation for a durable step key.
///
/// # Errors
///
/// Returns a [`FlowError`] when the work directory of the step cannot be read.
pub fn latest_workspace(root: &Path, key: &StepKey) -> FlowResult<Option<PathBuf>> {
    let parent = root.join("work").join(relative(key.as_str())?);
    if !parent.exists() {
        return Ok(None);
    }
    let mut latest = None;
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(number) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u64>().ok())
            && latest.as_ref().is_none_or(|(n, _)| number > *n)
        {
            latest = Some((number, entry.path()));
        }
    }
    Ok(latest.map(|(_, path)| path))
}
