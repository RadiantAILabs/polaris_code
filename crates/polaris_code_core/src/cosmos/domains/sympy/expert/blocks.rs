//! Deterministic verification of a proposed SymPy patch in its nearby tests.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use polaris::graph::predicate::BoxedDiscriminator;
use polaris::graph::{Graph, NodeId};
use polaris::shell::{ShellExecutor, ShellPermission, ShellRequest, ShellResponse};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris::system::system::SystemError;
use polaris_cosmos::composition::{
    BlockBuildError, BlockFactory, CaseKey, CompositionCatalogue, CompositionCatalogueError,
    DiscriminatorId, DiscriminatorKey, Effect,
};
use polaris_cosmos::contracts::{
    Assessment, CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef, Status,
};

use crate::cosmos::assess::site_assessment;
use crate::cosmos::behavior_version;
use crate::cosmos::gate_blocks::configurable::ProposedPatchIntent;
use crate::cosmos::gate_blocks::no_config_schema;
use crate::cosmos::payload::{
    AppliedEdit, CheckFailureClass, CodingWorkspace, EditSite, TestRunOutcome,
};

const MAX_OUTPUT: usize = 8 * 1024;
const SHELL_BUDGET: Duration = Duration::from_secs(120);
const CHECK_SCRIPT: &str = "sympy_scoped_check.py";

/// Returns the deterministic scoped-verification block key.
#[must_use]
pub fn run_scoped_verification_key() -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new("sympy.run-scoped-verification").expect("valid block ID"),
        behavior_version(),
    )
}

/// Returns the three-way verification discriminator key.
#[must_use]
pub fn verification_state_key() -> DiscriminatorKey {
    DiscriminatorKey::new(
        DiscriminatorId::new("sympy.verification-state").expect("valid discriminator ID"),
        behavior_version(),
    )
}

/// The block keys contributed by this variant.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![run_scoped_verification_key()]
}

/// Register the block and its closed three-way discriminator.
///
/// # Errors
/// Returns a catalogue error on duplicate registration.
pub fn register_all(catalogue: &mut CompositionCatalogue) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        run_scoped_verification_key(),
        Arc::new(RunScopedVerificationBlock),
        no_config_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.register_discriminator(
        verification_state_key(),
        BoxedDiscriminator::new::<VerificationState, _>(verification_state),
        ["passed", "failed", "unavailable"]
            .into_iter()
            .map(|s| CaseKey::new(s).expect("valid case label"))
            .collect::<BTreeSet<_>>(),
    )?;
    Ok(())
}

/// The closed state used by the `SymPy` verification switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationState {
    /// At least one check ran and every executed check passed.
    Passed,
    /// An executed check failed on behavior.
    Failed,
    /// No reliable check verdict could be established.
    Unavailable,
}

fn verification_state(state: &VerificationState) -> &'static str {
    match state {
        VerificationState::Passed => "passed",
        VerificationState::Failed => "failed",
        VerificationState::Unavailable => "unavailable",
    }
}

/// Runs a submitted check and up to three edited or adjacent `SymPy` test files.
pub struct RunScopedVerificationBlock;

impl BlockFactory for RunScopedVerificationBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        graph.add_system_node(run_scoped_verification);
        graph.add_system_node(assess_verification);
        Ok(graph.add_system_node(project_state))
    }
}

#[system]
async fn assess_verification(
    workspace: Res<CodingWorkspace>,
    outcome: Out<TestRunOutcome>,
) -> Result<Assessment<EditSite>, SystemError> {
    site_assessment(EditSite::new(
        workspace.subject_path().to_owned(),
        0,
        outcome.status,
    ))
    .map_err(|error| SystemError::ExecutionError(error.to_string()))
}

#[system]
async fn project_state(outcome: Out<TestRunOutcome>) -> VerificationState {
    match outcome.status {
        Status::Pass => VerificationState::Passed,
        Status::Issue => VerificationState::Failed,
        Status::Indeterminate | Status::NotApplicable => VerificationState::Unavailable,
    }
}

#[derive(Debug, Default)]
struct Targets {
    tests: Vec<String>,
    doctest: Option<String>,
}

fn validated_path(workspace: &CodingWorkspace, path: &str) -> Result<PathBuf, SystemError> {
    let parsed = Path::new(path);
    if !parsed
        .components()
        .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        || path.contains('\n')
        || path.contains('\r')
    {
        return Err(SystemError::ExecutionError(format!(
            "proposed patch path is not safe: {path:?}"
        )));
    }
    // Match the edit block's sandbox resolution, including absolute paths
    // rooted at the workspace, then use a relative path for test selection.
    let config = workspace.config();
    let root = config.working_dir.canonicalize().map_err(|error| {
        SystemError::ExecutionError(format!("could not resolve workspace root: {error}"))
    })?;
    let resolved = config.resolve_path(path).ok_or_else(|| {
        SystemError::ExecutionError(format!("proposed patch path escapes workspace: {path:?}"))
    })?;
    let relative = resolved.strip_prefix(&root).map_err(|_| {
        SystemError::ExecutionError(format!("proposed patch path escapes workspace: {path:?}"))
    })?;
    if relative.as_os_str().is_empty() {
        return Err(SystemError::ExecutionError(format!(
            "proposed patch path names the workspace root: {path:?}"
        )));
    }
    Ok(relative.to_path_buf())
}

fn select_targets(
    workspace: &CodingWorkspace,
    proposal: &ProposedPatchIntent,
) -> Result<Targets, SystemError> {
    let mut edited_tests = BTreeSet::new();
    let mut adjacent = BTreeSet::new();
    let mut doctest = BTreeSet::new();
    for edit in &proposal.patch.edits {
        let path = validated_path(workspace, edit.file())?;
        if !matches!(path.components().next(), Some(Component::Normal(first)) if first == "sympy") {
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "py") {
            continue;
        }
        let relative = path.to_string_lossy().into_owned();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if path
            .components()
            .any(|component| component.as_os_str() == "tests")
            && name.starts_with("test_")
        {
            edited_tests.insert(relative.clone());
        } else if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
            let test_path = path
                .parent()
                .unwrap_or(Path::new("sympy"))
                .join("tests")
                .join(format!("test_{stem}.py"));
            let test = test_path.to_string_lossy().into_owned();
            if workspace.read(&test).is_ok() {
                adjacent.insert(test);
            }
        }
        if (edit.find().contains(">>>") || edit.replace().contains(">>>"))
            && !name.starts_with("test_")
        {
            doctest.insert(relative);
        }
    }
    let mut tests = Vec::new();
    for path in edited_tests.into_iter().chain(adjacent) {
        if !tests.contains(&path) && tests.len() < 3 {
            tests.push(path);
        }
    }
    Ok(Targets {
        tests,
        doctest: doctest.into_iter().next(),
    })
}

fn bounded(text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    const MARKER: &str = "\n[output cut]\n";
    let mut head = 512;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (MAX_OUTPUT - head - MARKER.len());
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{MARKER}{}", &text[..head], &text[tail..])
}

fn outcome(status: Status, reason: impl AsRef<str>, detail: impl AsRef<str>) -> TestRunOutcome {
    TestRunOutcome {
        status,
        output: bounded(format!("{}\n{}", reason.as_ref(), detail.as_ref())),
        check_failure: match status {
            Status::Pass => None,
            Status::Issue => Some(CheckFailureClass::Behavior),
            Status::Indeterminate | Status::NotApplicable => {
                Some(CheckFailureClass::Infrastructure)
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Passed,
    Failed,
    Unavailable,
}

// SymPy's bin/test and bin/doctest print `tests finished: N passed` even
// when N is zero; a zero exit code by itself therefore proves nothing.
fn runner_verdict(exit: Option<i32>, timed_out: bool, output: &str) -> Verdict {
    if timed_out || exit.is_none() || matches!(exit, Some(126 | 127)) {
        return Verdict::Unavailable;
    }
    if output.contains("ModuleNotFoundError:") || output.contains("ImportError:") {
        return Verdict::Unavailable;
    }
    let Some(summary) = output.lines().find(|line| line.contains("tests finished:")) else {
        return Verdict::Unavailable;
    };
    let words: Vec<&str> = summary.split_whitespace().collect();
    let count = |label: &str| -> usize {
        words
            .iter()
            .position(|word| word.trim_end_matches(',') == label)
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| words[index].parse().ok())
            .unwrap_or(0)
    };
    if count("failed") > 0 {
        return Verdict::Failed;
    }
    if exit == Some(0) && count("passed") > 0 {
        Verdict::Passed
    } else {
        Verdict::Unavailable
    }
}

fn shell_arg(path: &str) -> String {
    format!("'{}'", path.replace('\'', "'\\''"))
}

async fn run_command(
    shell: &ShellExecutor,
    working_dir: &Path,
    command: &str,
    deadline: Instant,
) -> Result<polaris::shell::ExecutionResult, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("120-second verification budget exhausted".into());
    }
    if shell.check_permission(command) != ShellPermission::Allow {
        return Err(format!(
            "shell policy does not allow {command:?} unattended"
        ));
    }
    let request = ShellRequest::new(command)
        .with_working_dir(working_dir.to_path_buf())
        .with_timeout(remaining.as_secs().max(1));
    let response = tokio::time::timeout(remaining, shell.execute(request))
        .await
        .map_err(|_| "120-second verification budget exhausted".to_owned())?
        .map_err(|error| format!("could not run {command:?}: {error}"))?;
    match response {
        ShellResponse::Executed(result) => Ok(result),
        ShellResponse::ConfirmationRequired(_) => {
            Err(format!("shell confirmation required for {command:?}"))
        }
    }
}

#[system]
async fn run_scoped_verification(
    workspace: Res<CodingWorkspace>,
    shell: Res<ShellExecutor>,
    proposal: Out<ProposedPatchIntent>,
    _applied: Out<AppliedEdit>,
) -> Result<TestRunOutcome, SystemError> {
    let targets = select_targets(&workspace, &proposal)?;
    let working_dir = workspace.config().working_dir.as_path();
    let deadline = Instant::now() + SHELL_BUDGET;
    let mut ran = false;
    let mut report = format!(
        "selected tests: {:?}; doctest: {:?}",
        targets.tests, targets.doctest
    );
    if let Some(check) = proposal
        .patch
        .check
        .as_deref()
        .filter(|script| !script.trim().is_empty())
    {
        let dir = match crate::state::state_dir(working_dir) {
            Ok(dir) => dir,
            Err(error) => {
                return Ok(outcome(
                    Status::Indeterminate,
                    "check unavailable",
                    format!("{report}\ncould not prepare check scratch: {error}"),
                ));
            }
        };
        let script = if !check.contains('\n') && check.contains("\\n") {
            check.replace("\\n", "\n")
        } else {
            check.to_owned()
        };
        if let Err(error) = tokio::fs::write(dir.join(CHECK_SCRIPT), script).await {
            return Ok(outcome(
                Status::Indeterminate,
                "check unavailable",
                format!("{report}\ncould not write check scratch: {error}"),
            ));
        }
        let command = format!("python .polaris/{CHECK_SCRIPT}");
        let check_deadline = deadline.min(Instant::now() + Duration::from_secs(30));
        let result = match run_command(&shell, working_dir, &command, check_deadline).await {
            Ok(result) => result,
            Err(error) => {
                return Ok(outcome(
                    Status::Indeterminate,
                    "check unavailable",
                    format!("{report}\n{error}"),
                ));
            }
        };
        report.push_str(&format!(
            "\n{command}\n{}\n{}",
            result.stdout, result.stderr
        ));
        if result.timed_out
            || result.exit_code.is_none()
            || matches!(result.exit_code, Some(126 | 127))
            || result.stderr.contains("ModuleNotFoundError:")
            || result.stderr.contains("ImportError:")
            || (result.stderr.contains("SyntaxError:") && result.stderr.contains(CHECK_SCRIPT))
        {
            return Ok(outcome(Status::Indeterminate, "check unavailable", report));
        }
        if result.exit_code != Some(0) {
            return Ok(outcome(Status::Issue, "check failed", report));
        }
        ran = true;
    }

    let mut commands = Vec::new();
    if !targets.tests.is_empty() {
        commands.push(format!(
            "python bin/test {}",
            targets
                .tests
                .iter()
                .map(|s| shell_arg(s))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    if let Some(path) = targets.doctest {
        commands.push(format!("python bin/doctest {}", shell_arg(&path)));
    }
    for command in commands {
        let result = match run_command(&shell, working_dir, &command, deadline).await {
            Ok(result) => result,
            Err(error) => {
                return Ok(outcome(
                    Status::Indeterminate,
                    "verification unavailable",
                    format!("{report}\n{error}"),
                ));
            }
        };
        let output = format!("{}\n{}", result.stdout, result.stderr);
        report.push_str(&format!("\n{command}\n{output}"));
        match runner_verdict(result.exit_code, result.timed_out, &output) {
            Verdict::Passed => ran = true,
            Verdict::Failed => return Ok(outcome(Status::Issue, "SymPy tests failed", report)),
            Verdict::Unavailable => {
                return Ok(outcome(
                    Status::Indeterminate,
                    "SymPy runner gave no usable verdict",
                    report,
                ));
            }
        }
    }
    Ok(if ran {
        outcome(Status::Pass, "scoped verification passed", report)
    } else {
        outcome(Status::Indeterminate, "no check or tests executed", report)
    })
}

#[cfg(test)]
mod tests {
    use polaris::graph::GraphExecutor;
    use polaris::system::param::SystemContext;
    use polaris::system::resource::Resources;

    use super::*;
    use crate::cosmos::gate_blocks::configurable::{ModelConversation, PatchIntent};
    use crate::cosmos::task::{CodingIntent, CodingTask, ExactEdit, SuccessCheck};

    fn workspace() -> (tempfile::TempDir, CodingWorkspace) {
        let dir = tempfile::tempdir().expect("temporary SymPy tree");
        std::fs::create_dir_all(dir.path().join("sympy/core/tests")).expect("test directory");
        let task = CodingTask::from_statement(
            "sympy-verifier-fixture",
            dir.path(),
            "fix a SymPy operation",
            CodingIntent::Careful,
            SuccessCheck::command("true", Vec::<String>::new()).expect("command check"),
        )
        .expect("statement task");
        (dir, CodingWorkspace::for_task(&task))
    }

    #[test]
    fn runner_states_match_dev_container_output() {
        let pass = "sympy/core/tests/test_basic.py[22] ...................... [OK]\n================== tests finished: 22 passed, in 0.18 seconds ==================";
        let failed = "sympy/core/tests/test_polaris_demo.py[1] F [FAIL]\n============= tests finished: 0 passed, 1 failed, in 0.00 seconds ==============";
        let none =
            "================== tests finished: 0 passed, in 0.00 seconds ==================";
        assert_eq!(runner_verdict(Some(0), false, pass), Verdict::Passed);
        assert_eq!(runner_verdict(Some(1), false, failed), Verdict::Failed);
        assert_eq!(runner_verdict(Some(0), false, none), Verdict::Unavailable);
        assert_eq!(
            runner_verdict(Some(0), false, "unrecognised output"),
            Verdict::Unavailable
        );
        assert_eq!(
            runner_verdict(
                Some(1),
                false,
                &format!("ModuleNotFoundError: optional parser\n{failed}")
            ),
            Verdict::Unavailable
        );
        assert_eq!(runner_verdict(None, true, pass), Verdict::Unavailable);
    }

    #[test]
    fn bounded_output_keeps_the_late_failure() {
        let output = bounded(format!(
            "selected tests: test_basic.py\n{}\nAssertionError: late",
            "x".repeat(16_000)
        ));
        assert!(output.len() <= MAX_OUTPUT);
        assert!(output.contains("selected tests: test_basic.py"));
        assert!(output.contains("AssertionError: late"));
    }

    #[test]
    fn targets_prioritize_edited_tests_and_reject_escaping_paths() {
        let (dir, workspace) = workspace();
        std::fs::write(
            dir.path().join("sympy/core/tests/test_basic.py"),
            "def test_x(): pass",
        )
        .expect("adjacent test");
        let edits = vec![
            ExactEdit::new("sympy/core/basic.py", "old", "new").expect("source edit"),
            ExactEdit::new("sympy/core/tests/test_z.py", "old", "new").expect("test edit"),
            ExactEdit::new("sympy/core/tests/test_a.py", "old", "new").expect("test edit"),
            ExactEdit::new("doc/src/example.rst", "old", "new").expect("documentation edit"),
        ];
        let proposal = ProposedPatchIntent {
            patch: PatchIntent { edits, check: None },
            conversation: ModelConversation::default(),
        };
        assert_eq!(
            select_targets(&workspace, &proposal)
                .expect("targets")
                .tests,
            [
                "sympy/core/tests/test_a.py",
                "sympy/core/tests/test_z.py",
                "sympy/core/tests/test_basic.py"
            ]
        );
        let escaping = ProposedPatchIntent {
            patch: PatchIntent {
                edits: vec![ExactEdit::new("sympy/../escape.py", "old", "new").expect("edit")],
                check: None,
            },
            conversation: ModelConversation::default(),
        };
        assert!(select_targets(&workspace, &escaping).is_err());
    }

    #[test]
    fn absolute_workspace_paths_select_nearby_tests() {
        let (dir, workspace) = workspace();
        std::fs::write(
            dir.path().join("sympy/core/tests/test_basic.py"),
            "def test_x(): pass",
        )
        .expect("adjacent test");
        let source = dir.path().join("sympy/core/basic.py");
        let proposal = ProposedPatchIntent {
            patch: PatchIntent {
                edits: vec![
                    ExactEdit::new(source.to_string_lossy(), "old", "new").expect("absolute edit"),
                    ExactEdit::new("sympy/core/basic.py", "old", "new").expect("relative edit"),
                ],
                check: None,
            },
            conversation: ModelConversation::default(),
        };
        assert_eq!(
            select_targets(&workspace, &proposal)
                .expect("absolute workspace path")
                .tests,
            ["sympy/core/tests/test_basic.py"]
        );

        let outside = dir.path().parent().expect("parent").join("outside.py");
        let escaping = ProposedPatchIntent {
            patch: PatchIntent {
                edits: vec![
                    ExactEdit::new(outside.to_string_lossy(), "old", "new").expect("outside edit"),
                ],
                check: None,
            },
            conversation: ModelConversation::default(),
        };
        assert!(select_targets(&workspace, &escaping).is_err());
    }

    #[derive(Clone)]
    struct BlockResult {
        outcome: TestRunOutcome,
        state: VerificationState,
        assessment: Status,
    }

    #[system]
    async fn capture_verification(
        outcome: Out<TestRunOutcome>,
        state: Out<VerificationState>,
        assessment: Out<Assessment<EditSite>>,
    ) -> BlockResult {
        BlockResult {
            outcome: outcome.clone(),
            state: *state,
            assessment: assessment.status(),
        }
    }

    async fn execute_check(script: Option<&str>, allow: bool) -> BlockResult {
        use std::os::unix::fs::PermissionsExt;

        let (dir, workspace) = workspace();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("shim directory");
        let python = bin.join("python");
        std::fs::write(&python, "#!/bin/sh\nexec python3 \"$@\"\n").expect("python shim");
        let mut permissions = std::fs::metadata(&python)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&python, permissions).expect("executable shim");
        let mut shell_config = polaris::shell::ShellConfig::new()
            .with_working_dir(dir.path())
            .with_allowed_dirs(vec![dir.path().to_path_buf()])
            .with_path(format!("{}:/usr/bin:/bin", bin.display()));
        if allow {
            shell_config = shell_config.with_allowed_commands(vec!["python *".to_owned()]);
        }
        let shell = ShellExecutor::new(shell_config);
        let check = script.map(str::to_owned);
        let mut graph = Graph::new();
        graph.add_system_node(move || {
            let check = check.clone();
            async move {
                ProposedPatchIntent {
                    patch: PatchIntent {
                        edits: Vec::new(),
                        check,
                    },
                    conversation: ModelConversation::default(),
                }
            }
        });
        graph.add_system_node(|| async {
            AppliedEdit {
                outcomes: Vec::new(),
            }
        });
        RunScopedVerificationBlock
            .build(
                &mut graph,
                &ResolvedConfigurationRef::inline(
                    no_config_schema(),
                    polaris_cosmos::contracts::ConfigurationValue::Null,
                )
                .expect("no config"),
            )
            .expect("build block");
        graph.add_system_node(capture_verification);
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        let mut globals = Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(workspace);
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("verification graph runs");
        execution
            .output::<BlockResult>()
            .expect("captured block outputs")
            .clone()
    }

    #[tokio::test]
    async fn block_reports_pass_failure_and_unavailable() {
        let passed = execute_check(Some("assert 1 == 1"), true).await;
        assert_eq!(passed.outcome.status, Status::Pass);
        assert_eq!(passed.state, VerificationState::Passed);
        assert_eq!(passed.assessment, Status::Pass);
        let failed = execute_check(Some("assert 1 == 2"), true).await;
        assert_eq!(
            failed.outcome.status,
            Status::Issue,
            "{}",
            failed.outcome.output
        );
        assert_eq!(failed.state, VerificationState::Failed);
        assert_eq!(failed.assessment, Status::Issue);
        assert_eq!(verification_state(&failed.state), "failed");
        let unavailable = execute_check(None, true).await;
        assert_eq!(unavailable.outcome.status, Status::Indeterminate);
        assert_eq!(unavailable.state, VerificationState::Unavailable);
        assert_eq!(unavailable.assessment, Status::Indeterminate);
        assert_eq!(
            execute_check(Some("assert 1 == 1"), false)
                .await
                .outcome
                .status,
            Status::Indeterminate
        );
        assert_eq!(
            execute_check(Some("import no_such_sympy_verifier_dependency"), true)
                .await
                .outcome
                .status,
            Status::Indeterminate
        );
    }

    #[tokio::test]
    async fn a_changed_source_runs_its_adjacent_test_without_a_submitted_check() {
        use std::os::unix::fs::PermissionsExt;

        let (dir, workspace) = workspace();
        std::fs::write(dir.path().join("sympy/core/basic.py"), "old").expect("source file");
        std::fs::write(
            dir.path().join("sympy/core/tests/test_basic.py"),
            "def test_x(): pass",
        )
        .expect("test file");
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("bin directory");
        std::fs::write(bin.join("test"), "import sys\nassert sys.argv[1:] == ['sympy/core/tests/test_basic.py']\nprint('tests finished: 1 passed, in 0.01 seconds')\n")
            .expect("test runner");
        let shim = dir.path().join("python");
        std::fs::write(&shim, "#!/bin/sh\nexec python3 \"$@\"\n").expect("python shim");
        let mut permissions = std::fs::metadata(&shim)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&shim, permissions).expect("executable shim");
        let shell = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_path_buf()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", dir.path().display())),
        );
        let mut graph = Graph::new();
        graph.add_system_node(|| async {
            ProposedPatchIntent {
                patch: PatchIntent {
                    edits: vec![
                        ExactEdit::new("sympy/core/basic.py", "old", "new").expect("source edit"),
                    ],
                    check: None,
                },
                conversation: ModelConversation::default(),
            }
        });
        graph.add_system_node(|| async {
            AppliedEdit {
                outcomes: Vec::new(),
            }
        });
        RunScopedVerificationBlock
            .build(
                &mut graph,
                &ResolvedConfigurationRef::inline(
                    no_config_schema(),
                    polaris_cosmos::contracts::ConfigurationValue::Null,
                )
                .expect("no config"),
            )
            .expect("build block");
        graph.add_system_node(capture_verification);
        let mut globals = Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(workspace);
        let result = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("runner executes");
        let verdict = result.output::<BlockResult>().expect("verdict");
        assert_eq!(
            verdict.outcome.status,
            Status::Pass,
            "{}",
            verdict.outcome.output
        );
        assert_eq!(verdict.state, VerificationState::Passed);
        assert_eq!(verdict.assessment, Status::Pass);
        assert!(verdict.outcome.output.contains("test_basic.py"));
    }
}
