//! Deterministic Django regression verification after an exact-edit patch.

use std::collections::BTreeSet;
use std::path::{Component, Path};
use std::sync::Arc;

use polaris::graph::{Graph, NodeId};
use polaris::shell::{
    ExecutionResult, ShellExecutor, ShellPermission, ShellRequest, ShellResponse,
};
use polaris::system::param::{Out, Res};
use polaris::system::system;
use polaris_cosmos::composition::{
    BlockBuildError, BlockFactory, CompositionCatalogue, CompositionCatalogueError, Effect,
};
use polaris_cosmos::contracts::{
    CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef, Status,
};

use crate::cosmos::gate_blocks::configurable::{PatchIntent, ProposedPatchIntent};
use crate::cosmos::payload::{AppliedEdit, CheckFailureClass, CodingWorkspace, TestRunOutcome};

const MAX_LABELS: usize = 4;
const MAX_OUTPUT_BYTES: usize = 4_096;
const TEST_TIMEOUT_SECS: u64 = 120;
const CHECK_TIMEOUT_SECS: u64 = 30;
const CHECK_SCRIPT_NAME: &str = "django_expert_check.py";

/// Stable key for the Django-focused test gate.
#[must_use]
pub fn verify_regression_key() -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new("django.verify-regression").expect("valid block ID"),
        crate::cosmos::behavior_version(),
    )
}

/// The repair-round verifier permits an empty edit when the prior patch is
/// already applied and the model supplies a fresh check.
#[must_use]
pub fn verify_repair_key() -> CompositionBlockKey {
    CompositionBlockKey::new(
        CompositionBlockId::new("django.verify-regression-repair").expect("valid block ID"),
        crate::cosmos::behavior_version(),
    )
}

/// The two instances of this expert variant's verifier.
#[must_use]
pub fn keys() -> Vec<CompositionBlockKey> {
    vec![verify_regression_key(), verify_repair_key()]
}

/// Add the Django verifier to the shared sealed catalogue.
///
/// # Errors
/// Returns a duplicate-block error if another registration uses this key.
pub fn register_all(catalogue: &mut CompositionCatalogue) -> Result<(), CompositionCatalogueError> {
    catalogue.register_block(
        verify_regression_key(),
        Arc::new(VerifyRegressionBlock),
        crate::cosmos::gate_blocks::no_config_schema(),
        Effect::ReadOnly,
    )?;
    catalogue.register_block(
        verify_repair_key(),
        Arc::new(VerifyRepairBlock),
        crate::cosmos::gate_blocks::no_config_schema(),
        Effect::ReadOnly,
    )?;
    Ok(())
}

fn outcome(
    status: Status,
    class: Option<CheckFailureClass>,
    message: impl Into<String>,
) -> TestRunOutcome {
    TestRunOutcome {
        status,
        check_failure: class,
        output: tail(&message.into(), MAX_OUTPUT_BYTES),
    }
}

fn issue(class: CheckFailureClass, message: impl Into<String>) -> TestRunOutcome {
    outcome(Status::Issue, Some(class), message)
}

fn tail(value: &str, cap: usize) -> String {
    if value.len() <= cap {
        return value.to_owned();
    }
    const MARKER: &str = "… truncated prefix\n";
    let mut start = value.len() - (cap - MARKER.len());
    while !value.is_char_boundary(start) {
        start += 1;
    }
    format!("{MARKER}{}", &value[start..])
}

fn patch_landed(workspace: &CodingWorkspace, patch: &PatchIntent) -> bool {
    !patch.edits.is_empty()
        && patch.edits.iter().all(|edit| {
            workspace.read(edit.file()).is_ok_and(|actual| {
                actual.contains(edit.replace())
                    && (edit.replace().contains(edit.find()) || !actual.contains(edit.find()))
            })
        })
}

fn valid_identifier(part: &str) -> bool {
    let mut chars = part.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn test_labels(workspace: &CodingWorkspace, patch: &PatchIntent) -> Result<Vec<String>, String> {
    let mut labels = BTreeSet::new();
    for edit in &patch.edits {
        let path = Path::new(edit.file());
        let components: Vec<_> = path.components().collect();
        if components.first() != Some(&Component::Normal("tests".as_ref())) {
            continue;
        }
        if components
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(format!("invalid test path: {}", edit.file()));
        }
        let names: Option<Vec<&str>> = components
            .iter()
            .map(|part| part.as_os_str().to_str())
            .collect();
        let names = names.ok_or_else(|| format!("non-UTF-8 test path: {}", edit.file()))?;
        let Some(file) = names.last() else { continue };
        let Some(stem) = file.strip_suffix(".py") else {
            continue;
        };
        if names.len() < 3 || !(stem.starts_with("test") || stem == "tests") {
            continue;
        }
        if !names[1..names.len() - 1]
            .iter()
            .all(|name| valid_identifier(name))
            || !valid_identifier(stem)
        {
            return Err(format!("invalid test label component: {}", edit.file()));
        }
        workspace
            .read(edit.file())
            .map_err(|error| format!("test file is unavailable: {error}"))?;
        labels.insert(
            names[1..names.len() - 1]
                .iter()
                .copied()
                .chain(std::iter::once(stem))
                .collect::<Vec<_>>()
                .join("."),
        );
    }
    if labels.len() > MAX_LABELS {
        return Err(format!(
            "{} changed test modules exceed the {MAX_LABELS}-label limit",
            labels.len()
        ));
    }
    Ok(labels.into_iter().collect())
}

async fn run_command(
    shell: &ShellExecutor,
    cwd: &Path,
    command: String,
    timeout: u64,
) -> Result<ExecutionResult, String> {
    if shell.check_permission(&command) != ShellPermission::Allow {
        return Err(format!("shell policy cannot run '{command}' unattended"));
    }
    let request = ShellRequest::new(command)
        .with_working_dir(cwd.to_path_buf())
        .with_timeout(timeout);
    match shell.execute(request).await {
        Ok(ShellResponse::Executed(result)) => Ok(result),
        Ok(ShellResponse::ConfirmationRequired(_)) => {
            Err("shell requested interactive confirmation".to_owned())
        }
        Err(error) => Err(format!("could not run Python: {error}")),
    }
}

fn run_summary(text: &str) -> Option<(usize, usize)> {
    text.lines().find_map(|line| {
        let tail = line.trim().strip_prefix("Ran ")?;
        let count = tail.split_whitespace().next()?.parse::<usize>().ok()?;
        let skipped = text
            .rsplit_once("skipped=")
            .and_then(|(_, value)| {
                value
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .unwrap_or(0);
        Some((count, skipped))
    })
}

fn classify_test_result(
    exit_code: Option<i32>,
    timed_out: bool,
    stdout: &str,
    stderr: &str,
) -> TestRunOutcome {
    let combined = format!("{stdout}\n{stderr}");
    if timed_out || matches!(exit_code, None | Some(126 | 127)) {
        return issue(
            CheckFailureClass::Infrastructure,
            format!(
                "Django test command timed out or could not start\n{}",
                tail(&combined, 3_700)
            ),
        );
    }
    if combined.contains("ModuleNotFoundError:")
        || combined.contains("ImportError:")
        || combined.contains("No module named ")
    {
        return issue(
            CheckFailureClass::Infrastructure,
            format!(
                "Django test dependency or discovery failed\n{}",
                tail(&combined, 3_700)
            ),
        );
    }
    let summary = run_summary(&combined);
    if summary.is_some_and(|(count, _)| count == 0)
        || combined.contains("Found 0 test(s)")
        || summary.is_some_and(|(count, skipped)| count > 0 && count == skipped)
    {
        return issue(
            CheckFailureClass::Infrastructure,
            format!(
                "Django discovered no runnable tests\n{}",
                tail(&combined, 3_700)
            ),
        );
    }
    if exit_code == Some(0) {
        if summary.is_none() {
            return issue(
                CheckFailureClass::Infrastructure,
                format!(
                    "Django test result has no test-count summary\n{}",
                    tail(&combined, 3_700)
                ),
            );
        }
        return outcome(
            Status::Pass,
            None,
            format!("Django tests passed\n{}", tail(&combined, 3_700)),
        );
    }
    if summary.is_none() && !combined.contains("FAILED") && !combined.contains("FAIL:") {
        return issue(
            CheckFailureClass::Infrastructure,
            format!("Django test setup failed\n{}", tail(&combined, 3_700)),
        );
    }
    issue(
        CheckFailureClass::Behavior,
        format!("Django regression tests failed\n{}", tail(&combined, 3_700)),
    )
}

fn classify_check_result(result: &ExecutionResult) -> TestRunOutcome {
    let combined = format!("{}\n{}", result.stdout, result.stderr);
    if result.timed_out
        || matches!(result.exit_code, None | Some(126 | 127))
        || result.stderr.contains("ModuleNotFoundError:")
        || result.stderr.contains("ImportError:")
        || (result.stderr.contains("SyntaxError:") && result.stderr.contains(CHECK_SCRIPT_NAME))
    {
        return issue(
            CheckFailureClass::Infrastructure,
            format!("submitted check could not run\n{}", tail(&combined, 3_700)),
        );
    }
    if result.exit_code == Some(0) {
        outcome(
            Status::Pass,
            None,
            format!("submitted check passed\n{}", tail(&combined, 3_700)),
        )
    } else {
        issue(
            CheckFailureClass::Behavior,
            format!("submitted check failed\n{}", tail(&combined, 3_700)),
        )
    }
}

fn combine(first: Option<TestRunOutcome>, second: Option<TestRunOutcome>) -> TestRunOutcome {
    let checks: Vec<_> = first.into_iter().chain(second).collect();
    let class = checks
        .iter()
        .filter_map(|item| item.check_failure)
        .find(|class| *class == CheckFailureClass::Infrastructure)
        .or_else(|| checks.iter().find_map(|item| item.check_failure));
    let status = if checks.iter().all(|item| item.status == Status::Pass) {
        Status::Pass
    } else {
        Status::Issue
    };
    let output = checks
        .iter()
        .map(|item| tail(&item.output, 1_900))
        .collect::<Vec<_>>()
        .join("\n");
    outcome(status, class, output)
}

async fn verify_regression_inner(
    workspace: &CodingWorkspace,
    shell: &ShellExecutor,
    patch: &PatchIntent,
    submitted_check: Option<&str>,
) -> TestRunOutcome {
    verify_with_mode(workspace, shell, patch, submitted_check, false).await
}

async fn verify_with_mode(
    workspace: &CodingWorkspace,
    shell: &ShellExecutor,
    patch: &PatchIntent,
    submitted_check: Option<&str>,
    allow_empty_repair: bool,
) -> TestRunOutcome {
    if patch.edits.is_empty() {
        // The shared resume prompt explicitly allows a zero-edit repair when
        // the first round's patch is already on disk. Such a repair still
        // needs a submitted check; otherwise there is no behavior evidence.
        if !allow_empty_repair || submitted_check.is_none_or(|script| script.trim().is_empty()) {
            return issue(
                CheckFailureClass::Behavior,
                "empty patch without a repair-round behavior check",
            );
        }
    } else if !patch_landed(workspace, patch) {
        return issue(
            CheckFailureClass::Behavior,
            "nonempty patch did not land exactly",
        );
    }
    let labels = match test_labels(workspace, patch) {
        Ok(labels) => labels,
        Err(error) => return issue(CheckFailureClass::Infrastructure, error),
    };
    let cwd = workspace.config().working_dir.as_path();
    let tests = if labels.is_empty() {
        None
    } else {
        let command = format!(
            "python tests/runtests.py --parallel=1 --noinput {}",
            labels.join(" ")
        );
        Some(
            match run_command(shell, cwd, command, TEST_TIMEOUT_SECS).await {
                Ok(result) => {
                    let verdict = classify_test_result(
                        result.exit_code,
                        result.timed_out,
                        &result.stdout,
                        &result.stderr,
                    );
                    outcome(
                        verdict.status,
                        verdict.check_failure,
                        format!("labels: {}\n{}", labels.join(", "), verdict.output),
                    )
                }
                Err(error) => issue(CheckFailureClass::Infrastructure, error),
            },
        )
    };
    let check = match submitted_check {
        None => None,
        Some(script) if script.trim().is_empty() => Some(issue(
            CheckFailureClass::Behavior,
            "submitted check is blank",
        )),
        Some(script) => {
            let script = if !script.contains('\n') && script.contains("\\n") {
                script.replace("\\n", "\n")
            } else {
                script.to_owned()
            };
            let result = match crate::state::state_dir(cwd) {
                Ok(dir) => match tokio::fs::write(dir.join(CHECK_SCRIPT_NAME), script).await {
                    Ok(()) => {
                        run_command(
                            shell,
                            cwd,
                            format!("python .polaris/{CHECK_SCRIPT_NAME}"),
                            CHECK_TIMEOUT_SECS,
                        )
                        .await
                    }
                    Err(error) => Err(format!("could not write submitted check: {error}")),
                },
                Err(error) => Err(format!("could not prepare check directory: {error}")),
            };
            Some(match result {
                Ok(result) => classify_check_result(&result),
                Err(error) => issue(CheckFailureClass::Infrastructure, error),
            })
        }
    };
    if tests.is_none() && check.is_none() {
        return outcome(Status::Pass, None, "patch landed; no behavior check ran");
    }
    combine(tests, check)
}

#[system]
async fn verify_regression(
    workspace: Res<CodingWorkspace>,
    shell: Res<ShellExecutor>,
    proposal: Out<ProposedPatchIntent>,
    patch: Out<PatchIntent>,
    _applied: Out<AppliedEdit>,
) -> TestRunOutcome {
    verify_regression_inner(&workspace, &shell, &patch, proposal.patch.check.as_deref()).await
}

#[system]
async fn verify_repair(
    workspace: Res<CodingWorkspace>,
    shell: Res<ShellExecutor>,
    proposal: Out<ProposedPatchIntent>,
    patch: Out<PatchIntent>,
    _applied: Out<AppliedEdit>,
) -> TestRunOutcome {
    verify_with_mode(
        &workspace,
        &shell,
        &patch,
        proposal.patch.check.as_deref(),
        true,
    )
    .await
}

/// Runs changed Django test modules and an optional model-submitted check.
pub struct VerifyRegressionBlock;

impl BlockFactory for VerifyRegressionBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(verify_regression))
    }
}

/// The same check in a repair round, allowing a checked zero-edit repair.
pub struct VerifyRepairBlock;

impl BlockFactory for VerifyRepairBlock {
    fn build(
        &self,
        graph: &mut Graph,
        _config: &ResolvedConfigurationRef,
    ) -> Result<NodeId, BlockBuildError> {
        Ok(graph.add_system_node(verify_repair))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cosmos::task::ExactEdit;
    use crate::tools::code::CodeToolsConfig;

    fn fixture(
        path: &str,
        disk_content: &str,
    ) -> (tempfile::TempDir, CodingWorkspace, PatchIntent) {
        let dir = tempfile::tempdir().expect("temporary checkout");
        let file = dir.path().join(path);
        std::fs::create_dir_all(file.parent().expect("parent")).expect("test directory");
        std::fs::write(&file, disk_content).expect("test file");
        let workspace = CodingWorkspace::new(
            CodeToolsConfig::new(dir.path()),
            path,
            "before",
            "after",
            "",
        )
        .expect("workspace");
        let patch = PatchIntent {
            edits: vec![ExactEdit::new(path, "before", "after").expect("edit")],
            check: None,
        };
        (dir, workspace, patch)
    }

    fn shell(dir: &tempfile::TempDir, allow: bool) -> ShellExecutor {
        std::os::unix::fs::symlink("/usr/bin/python3", dir.path().join("python"))
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .expect("test Python alias");
        let config = polaris::shell::ShellConfig::new()
            .with_allowed_dirs(vec![dir.path().to_path_buf()])
            .with_allowed_commands(if allow {
                vec!["python *".to_owned()]
            } else {
                Vec::new()
            })
            .with_path(format!("{}:/usr/bin:/bin", dir.path().display()));
        ShellExecutor::new(config)
    }

    fn runner(dir: &tempfile::TempDir, script: &str) {
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests directory");
        std::fs::write(dir.path().join("tests/runtests.py"), script).expect("runner");
    }

    #[test]
    fn runner_output_states_are_classified() {
        let pass = classify_test_result(
            Some(0),
            false,
            "Found 1 test(s).\nRan 1 test in 0.01s\nOK",
            "",
        );
        assert_eq!(pass.status, Status::Pass);
        assert_eq!(pass.check_failure, None);
        let fail = classify_test_result(
            Some(1),
            false,
            "Ran 1 test in 0.01s\nFAILED (failures=1)",
            "",
        );
        assert_eq!(fail.check_failure, Some(CheckFailureClass::Behavior));
        let no_tests = classify_test_result(
            Some(0),
            false,
            "Found 0 test(s).\nRan 0 tests in 0.00s\nOK",
            "",
        );
        assert_eq!(
            no_tests.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        let skipped =
            classify_test_result(Some(0), false, "Ran 1 test in 0.00s\nOK (skipped=1)", "");
        assert_eq!(
            skipped.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        let dependency = classify_test_result(
            Some(1),
            false,
            "",
            "ModuleNotFoundError: No module named 'optional'",
        );
        assert_eq!(
            dependency.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        let timeout = classify_test_result(None, true, "", "");
        assert_eq!(
            timeout.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        let setup = classify_test_result(Some(1), false, "", "configuration failed");
        assert_eq!(setup.check_failure, Some(CheckFailureClass::Infrastructure));
    }

    #[test]
    fn check_and_combined_states_are_classified() {
        let failed = issue(CheckFailureClass::Behavior, "assertion failed");
        let infrastructure = issue(CheckFailureClass::Infrastructure, "could not run");
        assert_eq!(
            combine(Some(failed.clone()), None).check_failure,
            Some(CheckFailureClass::Behavior)
        );
        assert_eq!(
            combine(Some(failed), Some(infrastructure)).check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        assert_eq!(
            combine(None, Some(outcome(Status::Pass, None, "ok"))).status,
            Status::Pass
        );
        assert_eq!(
            issue(CheckFailureClass::Infrastructure, "shell policy denied").status,
            Status::Issue
        );
    }

    #[test]
    fn summary_parser_and_label_components_are_bounded() {
        assert_eq!(
            run_summary("Ran 3 tests in 0.1s\nOK (skipped=2)"),
            Some((3, 2))
        );
        assert!(valid_identifier("test_method"));
        assert!(!valid_identifier("test-method"));
        assert!(!valid_identifier("$(command)"));
        assert!(tail(&"x".repeat(5_000), 4_096).len() <= 4_096);
    }

    #[tokio::test]
    async fn changed_test_module_runs_and_passes() {
        let (dir, workspace, patch) = fixture("tests/demo/test_case.py", "after");
        runner(&dir, "print('Ran 1 test in 0.01s')\nprint('OK')\n");
        let result = verify_regression_inner(&workspace, &shell(&dir, true), &patch, None).await;
        assert_eq!(result.status, Status::Pass, "{}", result.output);
        assert!(result.output.contains("labels: demo.test_case"));
    }

    #[tokio::test]
    async fn changed_test_module_failure_is_repairable() {
        let (dir, workspace, patch) = fixture("tests/demo/test_case.py", "after");
        runner(
            &dir,
            "print('Ran 1 test in 0.01s')\nprint('FAILED (failures=1)')\nraise SystemExit(1)\n",
        );
        let result = verify_regression_inner(&workspace, &shell(&dir, true), &patch, None).await;
        assert_eq!(result.check_failure, Some(CheckFailureClass::Behavior));
    }

    #[tokio::test]
    async fn zero_tests_and_denied_shell_are_infrastructure_issues() {
        let (dir, workspace, patch) = fixture("tests/demo/test_case.py", "after");
        runner(&dir, "print('Ran 0 tests in 0.00s')\nprint('OK')\n");
        let zero = verify_regression_inner(&workspace, &shell(&dir, true), &patch, None).await;
        assert_eq!(zero.check_failure, Some(CheckFailureClass::Infrastructure));
        let denied = verify_regression_inner(&workspace, &shell(&dir, false), &patch, None).await;
        assert_eq!(
            denied.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        assert!(denied.output.contains("policy"));
    }

    #[tokio::test]
    async fn check_only_and_patch_fallback_preserve_generic_behavior() {
        let (dir, workspace, patch) = fixture("src.py", "after");
        let executor = shell(&dir, true);
        let fallback = verify_regression_inner(&workspace, &executor, &patch, None).await;
        assert_eq!(fallback.status, Status::Pass);
        assert!(fallback.output.contains("no behavior check ran"));
        let pass =
            verify_regression_inner(&workspace, &executor, &patch, Some("assert 1 == 1")).await;
        assert_eq!(pass.status, Status::Pass, "{}", pass.output);
        let fail =
            verify_regression_inner(&workspace, &executor, &patch, Some("assert 1 == 2")).await;
        assert_eq!(fail.check_failure, Some(CheckFailureClass::Behavior));
        let missing = verify_regression_inner(
            &workspace,
            &executor,
            &patch,
            Some("import module_that_does_not_exist_for_this_test"),
        )
        .await;
        assert_eq!(
            missing.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
        let blank = verify_regression_inner(&workspace, &executor, &patch, Some(" ")).await;
        assert_eq!(blank.status, Status::Issue);
        let denied =
            verify_regression_inner(&workspace, &shell(&dir, false), &patch, Some("pass")).await;
        assert_eq!(
            denied.check_failure,
            Some(CheckFailureClass::Infrastructure)
        );
    }

    #[tokio::test]
    async fn empty_or_unlanded_patch_is_a_behavior_issue() {
        let (dir, workspace, patch) = fixture("src.py", "before");
        let mismatch = verify_regression_inner(&workspace, &shell(&dir, true), &patch, None).await;
        assert_eq!(mismatch.check_failure, Some(CheckFailureClass::Behavior));
        let empty = PatchIntent {
            edits: Vec::new(),
            check: None,
        };
        let result = verify_regression_inner(&workspace, &shell(&dir, true), &empty, None).await;
        assert_eq!(result.check_failure, Some(CheckFailureClass::Behavior));
    }

    #[tokio::test]
    async fn repair_can_recheck_an_already_applied_patch_without_new_edits() {
        let (dir, workspace, _) = fixture("src.py", "after");
        let empty = PatchIntent {
            edits: Vec::new(),
            check: None,
        };
        let executor = shell(&dir, true);
        let pass =
            verify_with_mode(&workspace, &executor, &empty, Some("assert 1 == 1"), true).await;
        assert_eq!(pass.status, Status::Pass, "{}", pass.output);
        let fail =
            verify_with_mode(&workspace, &executor, &empty, Some("assert 1 == 2"), true).await;
        assert_eq!(fail.check_failure, Some(CheckFailureClass::Behavior));
        let unchecked = verify_with_mode(&workspace, &executor, &empty, None, true).await;
        assert_eq!(unchecked.status, Status::Issue);
        assert_eq!(unchecked.check_failure, Some(CheckFailureClass::Behavior));
    }

    #[test]
    fn too_many_changed_test_modules_are_reported_without_running() {
        let (dir, workspace, mut patch) = fixture("tests/demo/test_0.py", "after");
        for index in 1..=MAX_LABELS {
            let path = format!("tests/demo/test_{index}.py");
            std::fs::write(dir.path().join(&path), "after").expect("extra test file");
            patch
                .edits
                .push(ExactEdit::new(path, "before", "after").expect("extra edit"));
        }
        let labels = test_labels(&workspace, &patch);
        assert!(labels.is_err());
        assert!(labels.expect_err("overflow").contains("limit"));
    }
}
