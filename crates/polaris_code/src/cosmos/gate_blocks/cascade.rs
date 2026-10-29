//! Deterministic check and warm continuation for the lean cascade.

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path};
use std::process::Command;
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::shell::{ShellExecutor, ShellPermission, ShellRequest, ShellResponse};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockFactory, CompositionCatalogue, CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    AssessmentConclusion, CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef,
    Status, StopReason,
};

use super::behavior_version;
use super::configurable::{
    AutotestEvidence, ContextMode, ModelStepArtifact, ModelStepConfig, ModelStepHandoff,
    ModelStepProvider, WorkspaceView, model_propose_schema, run_with_handoff_conversation,
};
use super::no_config_schema;
use crate::cosmos::assess::{conclude_sites, site_assessment};
use crate::cosmos::payload::{CodingWorkspace, EditSite};
use crate::usage::TokenMeter;

const REPRO: &str = ".polaris/lean-repro.py";
const MAX_REASON: usize = 600;

fn key(id: &str) -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new(id).expect("stable cascade block ID"),
        behavior_version(),
    )
}

/// Capture the model-turn counter before the first editing tier.
#[derive(Clone, Debug)]
pub struct CascadeStart {
    turns: u64,
    total: u16,
    baseline: Result<BTreeMap<String, Option<Vec<u8>>>, String>,
}

/// One system check, with a bounded reason and remaining model-turn budget.
#[derive(Clone, Debug)]
pub struct CascadeCheck {
    /// True only when all three checks established their required evidence.
    pub passed: bool,
    /// Brief evidence or the first reason for continuing.
    pub reason: String,
    remaining: u16,
}

#[system]
async fn begin(
    config: Out<ModelStepConfig>,
    meter: Res<TokenMeter>,
    workspace: Res<CodingWorkspace>,
) -> Result<CascadeStart, SystemError> {
    let state = crate::state::state_dir(&workspace.config().working_dir)
        .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    tokio::fs::write(
        state.join("lean-repro.py"),
        "# Replace this placeholder with a focused assertion.\n",
    )
    .await
    .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    Ok(CascadeStart {
        turns: meter.snapshot("").num_turns,
        total: config.max_turns,
        baseline: initial_changes(&workspace.config().working_dir),
    })
}

#[system]
async fn check(
    workspace: Res<CodingWorkspace>,
    start: Out<CascadeStart>,
    _answer: Out<ModelStepArtifact>,
    meter: Res<TokenMeter>,
    shell: Res<ShellExecutor>,
    handoff: Res<ModelStepHandoff>,
) -> CascadeCheck {
    let used = meter.snapshot("").num_turns.saturating_sub(start.turns);
    let remaining = u64::from(start.total)
        .saturating_sub(used)
        .min(u64::from(u16::MAX)) as u16;
    let checked = match &start.baseline {
        Ok(baseline) => {
            check_workspace(&workspace, &shell, baseline, handoff.autotest().as_ref()).await
        }
        Err(error) => Err(format!("Initial tree could not be recorded: {error}")),
    };
    match checked {
        Ok(()) => CascadeCheck {
            passed: true,
            reason: "Patch, fail-before/pass-after reproduction, and nearby tests passed."
                .to_owned(),
            remaining,
        },
        Err(reason) => CascadeCheck {
            passed: false,
            reason: reason.chars().take(MAX_REASON).collect(),
            remaining,
        },
    }
}

#[system]
async fn continue_edit(
    workspace: Res<CodingWorkspace>,
    config: Out<ModelStepConfig>,
    check: Out<CascadeCheck>,
    provider: Res<ModelStepProvider>,
    meter: Res<TokenMeter>,
    mut handoff: Res<ModelStepHandoff>,
) -> Result<ModelStepArtifact, SystemError> {
    let mut config = (*config).clone();
    config.max_turns = check.remaining.max(1);
    config.context_mode = ContextMode::Continue;
    config.instruction = format!(
        "System check: {} Fix it, then submit a short answer. Reuse the work already in this conversation.",
        check.reason
    );
    let view = WorkspaceView::new(&workspace);
    Ok(
        run_with_handoff_conversation(&config, &provider, &view, &meter, &handoff)
            .await?
            .0,
    )
}

#[system]
async fn conclude_passed(check: Out<CascadeCheck>) -> Result<AssessmentConclusion, SystemError> {
    if !check.passed {
        return Err(SystemError::ExecutionError(
            "cascade pass conclusion requires all checks to pass".to_owned(),
        ));
    }
    let site = site_assessment(EditSite::new(
        "(workspace)".to_owned(),
        0,
        Status::Indeterminate,
    ))
    .map_err(|error| SystemError::ExecutionError(error.to_string()))?;
    conclude_sites(vec![site], Vec::new(), StopReason::PredicateSatisfied)
        .map_err(|error| SystemError::ExecutionError(error.to_string()))
}

struct BeginBlock;
impl BlockFactory for BeginBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let config = ModelStepConfig::decode(config)?;
        graph.add_system_node(move || {
            let config = config.clone();
            async move { config }
        });
        Ok(graph.add_system_node(begin))
    }
}

struct CheckBlock;
impl BlockFactory for CheckBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(check))
    }
}

struct ContinueBlock;
impl BlockFactory for ContinueBlock {
    fn build(
        &self,
        graph: &mut Graph,
        config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        let config = ModelStepConfig::decode(config)?;
        graph.add_system_node(move || {
            let config = config.clone();
            async move { config }
        });
        Ok(graph.add_system_node(continue_edit))
    }
}

struct ConcludePassedBlock;
impl BlockFactory for ConcludePassedBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(conclude_passed))
    }
}

/// Register the four cascade blocks.
pub fn register_all(catalogue: &mut CompositionCatalogue) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        begin_key(),
        Arc::new(BeginBlock),
        model_propose_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.register_block(
        check_key(),
        Arc::new(CheckBlock),
        no_config_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.register_block(
        continue_key(),
        Arc::new(ContinueBlock),
        model_propose_schema(),
        Effect::Mutating { repeat_safe: false },
    )?;
    catalogue.register_block(
        conclude_passed_key(),
        Arc::new(ConcludePassedBlock),
        no_config_schema(),
        Effect::ReadOnly,
    )?;
    Ok(())
}

/// Begin block key.
#[must_use]
pub fn begin_key() -> CompositionBlockKey {
    key("code.cascade-begin")
}
/// System check block key.
#[must_use]
pub fn check_key() -> CompositionBlockKey {
    key("code.cascade-check")
}
/// Warm continuation block key.
#[must_use]
pub fn continue_key() -> CompositionBlockKey {
    key("code.cascade-continue")
}
/// Passing conclusion block key.
#[must_use]
pub fn conclude_passed_key() -> CompositionBlockKey {
    key("code.cascade-conclude-passed")
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|error| format!("git unavailable: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed",
            args.first().copied().unwrap_or("")
        ));
    }
    Ok(output.stdout)
}

fn paths(bytes: &[u8]) -> Result<Vec<String>, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path).map_err(|_| "non-UTF-8 path".to_owned())?;
            if path.contains(['\n', '\r'])
                || !Path::new(path)
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err("unsafe path in git changes".to_owned());
            }
            Ok(path.to_owned())
        })
        .collect()
}

fn changed_paths(root: &Path) -> Result<Vec<String>, String> {
    let mut changed = paths(&git(root, &["diff", "HEAD", "--name-only", "-z"])?)?;
    changed.extend(paths(&git(
        root,
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )?)?);
    changed.retain(|path| !path.starts_with(".polaris/"));
    changed.sort();
    changed.dedup();
    Ok(changed)
}

fn file_bytes(root: &Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    let path = root.join(path);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => std::fs::read(path)
            .map(Some)
            .map_err(|error| error.to_string()),
        Ok(_) => Err("Changed path is not a regular file.".to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn tree_signature(root: &Path) -> Result<u64, String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    initial_changes(root)?.hash(&mut hasher);
    Ok(hasher.finish())
}

fn initial_changes(root: &Path) -> Result<BTreeMap<String, Option<Vec<u8>>>, String> {
    changed_paths(root)?
        .into_iter()
        .map(|path| {
            let bytes = file_bytes(root, &path)?;
            Ok((path, bytes))
        })
        .collect()
}

fn copy_tree(source: &Path, target: &Path) -> Result<(), String> {
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.depth() == 0
                || !matches!(
                    entry.file_name().to_str(),
                    Some(".git" | ".polaris" | ".venv" | "node_modules" | "target" | "__pycache__")
                )
        })
    {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.depth() == 0 {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(source)
            .map_err(|error| error.to_string())?;
        let dest = target.join(relative);
        if entry.file_type().is_dir() {
            std::fs::create_dir_all(&dest).map_err(|error| error.to_string())?;
        } else if entry.file_type().is_file() {
            std::fs::copy(entry.path(), &dest).map_err(|error| error.to_string())?;
        } else if entry.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(
                std::fs::read_link(entry.path()).map_err(|error| error.to_string())?,
                dest,
            )
            .map_err(|error| error.to_string())?;
            #[cfg(not(unix))]
            return Err("symlinked project files are unsupported on this host".to_owned());
        }
    }
    Ok(())
}

fn patched_copy(root: &Path) -> Result<tempfile::TempDir, String> {
    let state = crate::state::state_dir(root).map_err(|error| error.to_string())?;
    let scratch = tempfile::Builder::new()
        .prefix("cascade-check-")
        .tempdir_in(state)
        .map_err(|error| error.to_string())?;
    copy_tree(root, scratch.path())?;
    let repro = root.join(REPRO);
    let dest = scratch.path().join(REPRO);
    std::fs::create_dir_all(dest.parent().expect("repro has parent"))
        .map_err(|error| error.to_string())?;
    std::fs::copy(repro, dest).map_err(|error| error.to_string())?;
    Ok(scratch)
}

fn original_copy(
    root: &Path,
    changed: &[String],
    baseline: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<tempfile::TempDir, String> {
    let scratch = patched_copy(root)?;
    restore_initial_files(root, scratch.path(), changed, baseline)?;
    Ok(scratch)
}

fn restore_initial_files(
    root: &Path,
    destination: &Path,
    files: &[String],
    baseline: &BTreeMap<String, Option<Vec<u8>>>,
) -> Result<(), String> {
    for path in files {
        let dest = destination.join(path);
        if dest.is_symlink() {
            return Err("changed symlink cannot be compared with HEAD".to_owned());
        }
        let before = match baseline.get(path) {
            Some(bytes) => bytes.clone(),
            None => git(root, &["show", &format!("HEAD:{path}")]).ok(),
        };
        match before {
            Some(bytes) => {
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(dest, bytes).map_err(|error| error.to_string())?;
            }
            None => {
                if dest.exists() {
                    std::fs::remove_file(dest).map_err(|error| error.to_string())?;
                }
            }
        }
    }
    Ok(())
}

struct PythonResult {
    passed: bool,
    assertion_failed: bool,
}

async fn python(shell: &ShellExecutor, root: &Path, command: &str) -> Result<PythonResult, String> {
    if shell.check_permission(command) != ShellPermission::Allow {
        return Err(format!("shell policy does not permit {command}"));
    }
    let response = shell
        .execute(
            ShellRequest::new(command)
                .with_working_dir(root.to_path_buf())
                .with_timeout(60),
        )
        .await
        .map_err(|error| error.to_string())?;
    match response {
        ShellResponse::Executed(result) => {
            if result.timed_out || result.exit_code.is_none() {
                return Err(format!("{command} timed out or was interrupted"));
            }
            Ok(PythonResult {
                passed: result.exit_code == Some(0),
                assertion_failed: result.stderr.contains("AssertionError"),
            })
        }
        ShellResponse::ConfirmationRequired(_) => {
            Err(format!("confirmation required for {command}"))
        }
    }
}

fn quoted(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

fn nearby_tests(root: &Path, changed: &[String]) -> Result<Vec<String>, String> {
    let tracked = paths(&git(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?)?;
    let mut tests = Vec::new();
    for source in changed.iter().filter(|path| path.ends_with(".py")) {
        if source.starts_with("tests/") || source.contains("/tests/") {
            if tracked.contains(source) {
                tests.push(source.clone());
            }
            continue;
        }
        let Some(stem) = Path::new(source).file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let sought = format!("test_{stem}.py");
        let plural = if let Some(prefix) = stem.strip_suffix('y') {
            format!("{prefix}ies")
        } else {
            format!("{stem}s")
        };
        for test in &tracked {
            if (test.ends_with(&sought)
                || test.contains(&format!("/{stem}/"))
                || test.contains(&format!("/{plural}/")))
                && (test.starts_with("tests/") || test.contains("/tests/"))
                && test.ends_with(".py")
                && Path::new(test)
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("test_"))
            {
                tests.push(test.clone());
            }
        }
    }
    tests.sort();
    tests.dedup();
    tests.truncate(3);
    Ok(tests)
}

fn may_reuse_nearby_tests(
    root: &Path,
    baseline: &BTreeMap<String, Option<Vec<u8>>>,
    tests: &[String],
    evidence: Option<&AutotestEvidence>,
) -> bool {
    let Some(evidence) = evidence else {
        return false;
    };
    if !tests.iter().all(|test| evidence.targets.contains(test))
        || tree_signature(root).ok() != Some(evidence.tree_signature)
    {
        return false;
    }
    // Cascade restores existing tests in the disposable copy. A test changed
    // during the editing tier is therefore not equivalent to the autotest run.
    tests.iter().all(|test| {
        let original = baseline
            .get(test)
            .cloned()
            .unwrap_or_else(|| git(root, &["show", &format!("HEAD:{test}")]).ok());
        file_bytes(root, test).ok() == Some(original)
    })
}

async fn check_workspace(
    workspace: &CodingWorkspace,
    shell: &ShellExecutor,
    baseline: &BTreeMap<String, Option<Vec<u8>>>,
    autotest: Option<&AutotestEvidence>,
) -> Result<(), String> {
    let root = workspace.config().working_dir.as_path();
    let mut changed = changed_paths(root)?;
    changed.extend(baseline.keys().cloned());
    changed.sort();
    changed.dedup();
    changed.retain(|path| {
        let before = baseline
            .get(path)
            .cloned()
            .unwrap_or_else(|| git(root, &["show", &format!("HEAD:{path}")]).ok());
        match file_bytes(root, path) {
            Ok(after) => before != after,
            _ => true,
        }
    });
    if changed.is_empty() {
        return Err("No non-empty patch was found.".to_owned());
    }
    let repro = root.join(REPRO);
    let metadata = std::fs::symlink_metadata(&repro)
        .map_err(|_| format!("Missing reproduction script at {REPRO}."))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 16_384 {
        return Err("Reproduction script must be a regular file of at most 16 KiB.".to_owned());
    }
    let script = std::fs::read_to_string(&repro)
        .map_err(|error| format!("Could not read reproduction: {error}"))?;
    if !script.contains("assert ") || script.contains("# Replace this placeholder") {
        return Err("Reproduction must replace the placeholder with an assertion.".to_owned());
    }
    let original = original_copy(root, &changed, baseline)
        .map_err(|error| format!("Original-tree check unavailable: {error}"))?;
    let command = format!(
        "python -c 'import sys,runpy;sys.path.insert(0,\".\");runpy.run_path(\"{REPRO}\",run_name=\"__main__\")'"
    );
    let before = python(shell, original.path(), &command).await?;
    if before.passed {
        return Err("Reproduction also passes on the original tree.".to_owned());
    }
    if !before.assertion_failed {
        return Err("Original-tree reproduction failed without an AssertionError.".to_owned());
    }
    let patched =
        patched_copy(root).map_err(|error| format!("Patched-tree check unavailable: {error}"))?;
    if !python(shell, patched.path(), &command).await?.passed {
        return Err("Reproduction does not pass on the patched tree.".to_owned());
    }
    let tests = nearby_tests(root, &changed)?;
    if tests.is_empty() {
        return Err("No existing nearby Python test was found.".to_owned());
    }
    restore_initial_files(root, patched.path(), &tests, baseline)
        .map_err(|error| format!("Could not restore existing tests: {error}"))?;
    if may_reuse_nearby_tests(root, baseline, &tests, autotest) {
        return Ok(());
    }
    let command = format!(
        "python -m pytest {}",
        tests
            .iter()
            .map(|path| quoted(path))
            .collect::<Vec<_>>()
            .join(" ")
    );
    if !python(shell, patched.path(), &command).await?.passed {
        return Err(format!("Nearby tests failed: {}", tests.join(", ")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};

    fn git_ok(root: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn check_requires_real_fail_before_and_preserves_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a - b\n").unwrap();
        std::fs::write(
            root.join("tests/test_calc.py"),
            "from calc import add\nassert add(2, 3) == 5\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pytest.py"),
            "import runpy, sys\nfrom pathlib import Path\nrunpy.run_path(sys.argv[1], run_name='__main__')\nPath('probe.txt').write_text('ran')\n",
        )
        .unwrap();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["add", "calc.py", "tests/test_calc.py", "pytest.py"]);
        git_ok(
            root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        );
        std::fs::write(root.join("calc.py"), "def add(a, b):\n    return a + b\n").unwrap();
        let state = crate::state::state_dir(root).unwrap();
        std::fs::write(
            state.join("lean-repro.py"),
            "from calc import add\nassert add(2, 3) == 5\n",
        )
        .unwrap();
        let shim = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            // Resolve the interpreter behind macOS's /usr/bin/python3 launcher.
            let python = Command::new("python3")
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
                .expect("discover test Python");
            assert!(python.status.success(), "test Python discovery failed");
            let executable = String::from_utf8(python.stdout).expect("Python executable path");
            std::os::unix::fs::symlink(executable.trim(), shim.path().join("python")).unwrap();
        }
        let shell = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(root)
                .with_allowed_dirs(vec![root.to_owned()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", shim.path().display()))
                .disable_cache(),
        );
        let task = CodingTask::from_statement(
            "cascade-check-test",
            root,
            "fix add",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .unwrap();
        let workspace = CodingWorkspace::for_task(&task);
        let patched = std::fs::read(root.join("calc.py")).unwrap();
        check_workspace(&workspace, &shell, &BTreeMap::new(), None)
            .await
            .unwrap();
        let evidence = AutotestEvidence {
            targets: vec!["tests/test_calc.py".to_owned()],
            tree_signature: tree_signature(root).unwrap(),
        };
        assert!(may_reuse_nearby_tests(
            root,
            &BTreeMap::new(),
            &evidence.targets,
            Some(&evidence)
        ));
        let repro_only = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(root)
                .with_allowed_dirs(vec![root.to_owned()])
                .with_allowed_commands(vec!["python -c *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", shim.path().display()))
                .disable_cache(),
        );
        check_workspace(&workspace, &repro_only, &BTreeMap::new(), Some(&evidence))
            .await
            .expect("matching passing autotest makes the nearby pytest run unnecessary");
        let already_dirty = initial_changes(root).unwrap();
        assert!(
            check_workspace(&workspace, &shell, &already_dirty, None)
                .await
                .unwrap_err()
                .contains("No non-empty patch")
        );
        assert_eq!(std::fs::read(root.join("calc.py")).unwrap(), patched);
        assert!(!root.join("probe.txt").exists());
        std::fs::write(root.join("tests/test_calc.py"), "assert False\n").unwrap();
        assert!(!may_reuse_nearby_tests(
            root,
            &BTreeMap::new(),
            &evidence.targets,
            Some(&evidence)
        ));
        check_workspace(&workspace, &shell, &BTreeMap::new(), None)
            .await
            .expect("the original existing test must run, not tier-one's rewrite");
        assert_eq!(
            std::fs::read_to_string(root.join("tests/test_calc.py")).unwrap(),
            "assert False\n"
        );
        assert!(!std::fs::read_dir(&state).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("cascade-check-")
        }));
        std::fs::write(state.join("lean-repro.py"), "assert 1 == 1\n").unwrap();
        assert!(
            check_workspace(&workspace, &shell, &BTreeMap::new(), None)
                .await
                .unwrap_err()
                .contains("also passes")
        );
        assert_eq!(std::fs::read(root.join("calc.py")).unwrap(), patched);
    }
}
