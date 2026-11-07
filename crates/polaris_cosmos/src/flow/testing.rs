//! File-backed scripted sessions for offline graph tests.
use super::{
    AgentFailure, AgentReport, AgentRequest, StopCause, StopFlag, TaskRunner, Usage, io::*,
    staging::copy_snapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
/// One scripted role response.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScriptEntry {
    /// Stop name, defaulting to finished.
    pub stop: String,
    /// Optional measured usage override.
    pub usage: Option<Usage>,
    /// Final response text.
    pub final_text: String,
    /// Simulated latency in milliseconds.
    pub delay_ms: u64,
    /// setup, unmeasured or internal failure.
    pub fail: Option<String>,
    /// Match only step keys containing this substring.
    pub when_step: Option<String>,
    /// Directory whose files are copied into the call workspace.
    #[serde(skip)]
    pub files: Option<PathBuf>,
}
/// Merged default usage and role entries of an offline scenario.
#[derive(Clone, Debug, Default)]
pub struct Scenario {
    /// Default usage for entries without an override.
    pub usage: Usage,
    /// Ordered per-role entries.
    pub roles: BTreeMap<String, Vec<ScriptEntry>>,
}
impl Scenario {
    /// Loads included scenarios first; this directory's role definitions win.
    pub fn load(root: &Path) -> FlowResult<Self> {
        Self::load_stack(root, &mut Vec::new())
    }
    fn load_stack(root: &Path, stack: &mut Vec<PathBuf>) -> FlowResult<Self> {
        let root = root.canonicalize()?;
        if stack.contains(&root) {
            return Err(FlowError::invalid("scenario include cycle"));
        }
        stack.push(root.clone());
        let text = fs::read_to_string(root.join("scenario.toml"))?;
        let doc: toml::Value =
            toml::from_str(&text).map_err(|e| FlowError::invalid(e.to_string()))?;
        let default = doc.get("default");
        let include = doc
            .get("include")
            .or_else(|| default.and_then(|v| v.get("include")));
        let mut scenario = Self::default();
        if let Some(includes) = include.and_then(toml::Value::as_array) {
            for include in includes {
                let path = include
                    .as_str()
                    .ok_or_else(|| FlowError::invalid("scenario include must be a path"))?;
                let other = Self::load_stack(&root.join(path), stack)?;
                scenario.roles.extend(other.roles);
                scenario.usage = other.usage;
            }
        }
        if let Some(usage) = default.and_then(|v| v.get("usage")) {
            scenario.usage = usage
                .clone()
                .try_into()
                .map_err(|e| FlowError::invalid(e.to_string()))?;
        }
        if root.join("roles").exists() {
            let mut roles = fs::read_dir(root.join("roles"))?
                .map(|e| e.map(|e| e.path()))
                .collect::<Result<Vec<_>, _>>()?;
            roles.sort();
            for role_dir in roles {
                if !role_dir.is_dir() {
                    continue;
                }
                let name = role_dir
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .ok_or_else(|| FlowError::invalid("scenario role folder has no name"))?;
                let mut files = fs::read_dir(&role_dir)?
                    .map(|e| e.map(|e| e.path()))
                    .collect::<Result<Vec<_>, _>>()?;
                files.sort();
                let mut entries = Vec::new();
                for path in files {
                    if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                        continue;
                    }
                    let mut entry: ScriptEntry = toml::from_str(&fs::read_to_string(&path)?)
                        .map_err(|e| FlowError::invalid(format!("{}: {e}", path.display())))?;
                    let folder = path.with_extension("");
                    if folder.is_dir() {
                        entry.files = Some(folder);
                    }
                    entries.push(entry);
                }
                scenario.roles.insert(name, entries);
            }
        }
        stack.pop();
        Ok(scenario)
    }
}
/// Offline runner which records every request and never calls a model or process.
#[derive(Debug)]
pub struct ScriptedRunner {
    scenario: Scenario,
    output: PathBuf,
    counts: Mutex<BTreeMap<String, usize>>,
    stop: StopFlag,
}
impl ScriptedRunner {
    /// Creates a runner from a scenario directory.
    pub fn load(root: &Path, output: &Path) -> FlowResult<Self> {
        Self::new(Scenario::load(root)?, output)
    }
    /// Creates a runner from in-memory entries and restores role ordinals from calls.jsonl.
    pub fn new(scenario: Scenario, output: &Path) -> FlowResult<Self> {
        fs::create_dir_all(output)?;
        let mut counts = BTreeMap::new();
        let calls = output.join("calls.jsonl");
        if calls.exists() {
            for line in fs::read_to_string(calls)?.lines() {
                let value: serde_json::Value = serde_json::from_str(line)?;
                if value["played"] == true
                    && let Some(role) = value["role"].as_str()
                {
                    *counts.entry(role.into()).or_default() += 1;
                }
            }
        }
        Ok(Self {
            scenario,
            output: output.into(),
            counts: Mutex::new(counts),
            stop: StopFlag::default(),
        })
    }
    /// Shares the workflow's cooperative stop flag.
    pub fn with_stop(mut self, flag: StopFlag) -> Self {
        self.stop = flag;
        self
    }
    fn interrupted(&self) -> AgentReport {
        AgentReport {
            stop: StopCause::Interrupted,
            usage: Usage::default(),
            final_text: String::new(),
            transcript: None,
        }
    }
}
#[async_trait::async_trait]
impl TaskRunner for ScriptedRunner {
    async fn run(&self, req: AgentRequest) -> Result<AgentReport, AgentFailure> {
        let selected = {
            let mut counts = self.counts.guard();
            let played = !self.stop.is_requested();
            let ordinal = *counts.get(&req.role).unwrap_or(&0);
            let mut line =
                serde_json::to_value(&req).map_err(|e| AgentFailure::Internal(e.to_string()))?;
            line["played"] = serde_json::json!(played);
            line["ordinal"] = serde_json::json!(ordinal + 1);
            append_json(&self.output.join("calls.jsonl"), &line)
                .map_err(|e| AgentFailure::Internal(e.to_string()))?;
            if !played {
                return Ok(self.interrupted());
            }
            let entries = self.scenario.roles.get(&req.role).ok_or_else(|| {
                AgentFailure::Setup(format!(
                    "no scripted role {} at step {}",
                    req.role, req.step
                ))
            })?;
            let matching = entries
                .iter()
                .filter(|e| {
                    e.when_step
                        .as_ref()
                        .is_none_or(|s| req.step.as_str().contains(s))
                })
                .collect::<Vec<_>>();
            let entry = matching
                .get(ordinal)
                .or_else(|| matching.last())
                .ok_or_else(|| {
                    AgentFailure::Setup(format!(
                        "no scripted entry for role {} at step {}",
                        req.role, req.step
                    ))
                })?;
            counts.insert(req.role.clone(), ordinal + 1);
            (*entry).clone()
        };
        if selected.delay_ms > 0 {
            tokio::select! {_ = self.stop.requested()=>return Ok(self.interrupted()),_ = tokio::time::sleep(req.limits.timeout)=>return Ok(AgentReport{stop:StopCause::Timeout,usage:selected.usage.clone().unwrap_or_else(||self.scenario.usage.clone()),final_text:String::new(),transcript:None}),_ = tokio::time::sleep(Duration::from_millis(selected.delay_ms))=>{}}
        }
        if self.stop.is_requested() {
            return Ok(self.interrupted());
        }
        if let Some(fail) = selected.fail {
            return Err(match fail.as_str() {
                "setup" => AgentFailure::Setup("scripted setup failure".into()),
                "unmeasured" => AgentFailure::Unmeasured("scripted unmeasured failure".into()),
                "internal" => AgentFailure::Internal("scripted internal failure".into()),
                _ => AgentFailure::Internal(format!("unknown scripted failure {fail}")),
            });
        }
        if let Some(files) = selected.files {
            for entry in fs::read_dir(&files).map_err(|e| AgentFailure::Internal(e.to_string()))? {
                let entry = entry.map_err(|e| AgentFailure::Internal(e.to_string()))?;
                copy_snapshot(
                    &entry.path(),
                    &req.workspace.join(entry.file_name()),
                    false,
                    &[files.as_path()],
                )
                .map_err(|e| AgentFailure::Internal(e.to_string()))?;
            }
        }
        let stop = match selected.stop.as_str() {
            "" | "finished" => StopCause::Finished,
            "turn_cap" => StopCause::TurnCap,
            "cost_cap" => StopCause::CostCap,
            "timeout" => StopCause::Timeout,
            "interrupted" => StopCause::Interrupted,
            s if s.starts_with("model_error") => StopCause::ModelError(s.into()),
            s => return Err(AgentFailure::Internal(format!("unknown scripted stop {s}"))),
        };
        Ok(AgentReport {
            stop,
            usage: selected
                .usage
                .unwrap_or_else(|| self.scenario.usage.clone()),
            final_text: selected.final_text,
            transcript: None,
        })
    }
}
/// Shared offline runner handle convenient for tests.
pub type SharedScriptedRunner = Arc<ScriptedRunner>;
