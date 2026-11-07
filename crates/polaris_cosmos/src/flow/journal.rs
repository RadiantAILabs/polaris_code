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
/// A durable step output; only `done` records are restored.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepRecord {
    /// Stable step identity.
    pub key: StepKey,
    /// Completion status.
    pub status: String,
    /// UTC start time.
    pub started: String,
    /// UTC completion time.
    pub ended: String,
    /// Serialized typed output.
    pub output: serde_json::Value,
}
/// Global durable completion journal shared by graph branches.
///
/// # Scope
/// Global because branches share durable step keys.
/// # Provided By
/// Workflow boot wiring using `StepJournal::open`.
/// # Access
/// `Res<StepJournal>`; writes and folder allocation hold an internal mutex.
/// # Alternatives
/// Unmemoized systems run every time.
/// # Example
/// ```ignore
/// async fn inspect(journal: Res<StepJournal>) -> bool { journal.get(&StepKey::new("check")).is_some() }
/// ```
#[derive(Clone)]
pub struct StepJournal {
    root: PathBuf,
    lock: Arc<Mutex<()>>,
}
impl GlobalResource for StepJournal {}
impl StepJournal {
    /// Opens a journal rooted in a workflow directory.
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
        self.read(key).ok().flatten().filter(|r| r.status == "done")
    }
    /// Reads a record, distinguishing corruption from absence.
    pub fn read(&self, key: &StepKey) -> FlowResult<Option<StepRecord>> {
        let path = self.path(key)?;
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(read_json(&path)?))
    }
    /// Publishes a completed output and appends its digest to the journal.
    pub fn put(&self, key: &StepKey, output: serde_json::Value) -> FlowResult<()> {
        let _lock = self.lock.lock().unwrap();
        let now = timestamp();
        let record = StepRecord {
            key: key.clone(),
            status: "done".into(),
            started: self
                .read(key)?
                .map_or_else(|| now.clone(), |record| record.started),
            ended: now.clone(),
            output,
        };
        write_json(&self.path(key)?, &record)?;
        append_json(
            &self.root.join("journal.jsonl"),
            &serde_json::json!({"ts":now,"key":key,"state":"done","digest":digest(&serde_json::to_vec(&record.output)?)}),
        )
    }
    /// Appends a non-completion step edge.
    pub fn edge(&self, key: &StepKey, state: &str) -> FlowResult<()> {
        let _lock = self.lock.lock().unwrap();
        let now = timestamp();
        let started = if state == "started" {
            now.clone()
        } else {
            self.read(key)?
                .map_or_else(|| now.clone(), |record| record.started)
        };
        write_json(
            &self.path(key)?,
            &StepRecord {
                key: key.clone(),
                status: state.into(),
                started,
                ended: if state == "started" {
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
    pub fn workspace(&self, key: &StepKey) -> FlowResult<PathBuf> {
        let _lock = self.lock.lock().unwrap();
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
    pub fn abandon(&self, path: &Path) -> FlowResult<()> {
        let _lock = self.lock.lock().unwrap();
        if !path.exists() {
            return Ok(());
        }
        let relative = path
            .strip_prefix(self.root.join("work"))
            .map_err(|_| FlowError("abandoned folder outside work root".into()))?;
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
/// Use a distinct key for each loop iteration. The body is ordinary graph wiring.
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
                let record = journal.read(&key)?.filter(|r| r.status == "done");
                if record.is_none() {
                    journal.edge(&key, "started")?;
                }
                Ok(MemoProbe { key, record })
            })
        },
    ));
    memo_branch::<T>(graph, body)
}
/// Memoizes a body using the preceding system's runtime `Out<StepKey>`.
/// This keeps per-item and per-iteration identities in graph data rather than shape.
pub fn memo_current<T: Serialize + DeserializeOwned + Clone + Send + Sync + 'static>(
    body: Graph,
) -> Graph {
    let mut graph = Graph::new();
    graph.add_system(super::bind::<(Out<StepKey>, Res<StepJournal>), _, _>(
        "memo.current",
        |(key, journal), _| {
            Box::pin(async move {
                let key = (*key).clone();
                let record = journal.read(&key)?.filter(|r| r.status == "done");
                if record.is_none() {
                    journal.edge(&key, "started")?;
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
                        serde_json::from_value(probe.record.as_ref().unwrap().output.clone())
                            .map_err(|e| SystemError::ExecutionError(e.to_string()))
                    })
                },
            ));
        },
        |g| {
            g.append(body).expect("fresh memo body");
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
                        if control.is_some_and(|control| {
                            control.halt_reason().as_deref() == Some("interrupted")
                        }) {
                            journal.edge(&probe.key, "failed")?;
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
