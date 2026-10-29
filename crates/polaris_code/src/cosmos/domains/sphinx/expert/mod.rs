//! Sphinx's expert-research domain variant: the `expert` arm's plan and notes
//! text — the same notes text [`super::super::expert_notes`] hands to the
//! `generic-notes` arm.
//!
//! [`NOTES`] carries the researched Sphinx notes (identical to the
//! `generic-notes` arm's copy). [`plan`] is
//! [`planner::statement_only_plan_body`] with [`NOTES`] appended, *except* in
//! the one shape the Sphinx research targets — a shell is available to run a
//! model-authored script but there is no project test runner
//! (`facts.shell_available && !facts.test_runner_available`) — where it
//! substitutes a Sphinx-specific self-check gate
//! ([`sphinx_self_check_gated_expression`]) for the generic one. See
//! `cosmos/domains/README.md` for how to test a change here.
//!
//! # Why this isn't `planner::self_check_gated_expression` with a config knob
//!
//! That function (and the private `no_config_id`/`step_expr` helpers it and
//! [`planner::statement_only_plan_body`] use) is a plain, non-`pub` `fn` in
//! `cosmos::planner` — unreachable from this sibling module. [`plan`]
//! instead harvests the correctly-built model-step configurations from
//! [`planner::statement_only_plan_body`] (guaranteeing this variant's
//! `propose-edit`/`propose-edit-repair` instructions stay byte-identical to
//! the `generic-notes` arm's, whenever it carries the same notes), and — only
//! in the one shape above — replaces its returned expression with
//! [`sphinx_self_check_gated_expression`], a local reimplementation of
//! `planner::self_check_gated_expression`'s exact shape with three
//! substitutions. [`no_config_id`]/[`step_expr`] below are local copies of
//! the same-named private helpers in `planner.rs`, needed to build that
//! expression; see their doc comments for the exact source cited.
//!
//! # Shape of the self-check gate
//!
//! This file follows the shape of the generic code
//! (`planner::self_check_gated_expression`): one shared final `conclude` step
//! and a single `check-needs-repair` `Choose`, with everything else staying
//! exactly as the generic plan uses it. This yields 14 `Step` instances.
//!
//! # The `run_python_script`/`PythonRunOutcome`/`classify_check_failure` gap
//!
//! `gate_blocks::configurable::run_python_script` and its `PythonRunOutcome`
//! return type, and `gate_blocks::repair::classify_check_failure`'s
//! `&PythonRunOutcome` parameter, are all `pub(super)`-scoped to
//! `gate_blocks` and its descendants — unreachable from this sibling module
//! even if `classify_check_failure` itself were widened (widening only the
//! function, not its parameter type, leaves it unusable). [`blocks`]
//! below carries local reimplementations of the shell-execution helper and
//! the failure classifier instead of widening those items.

use polaris_cosmos::composition::{BlockInstanceId, ConfigId, GateExpr};
use polaris_cosmos::contracts::RevisionAttempt;

use crate::cosmos::gate_blocks::{configurable, existing, predicates, repair};
use crate::cosmos::planner::{self, PlannerError, PlannerFacts, StatementPlanBody};

/// Domain notes injected into every statement-only model step's instruction
/// for the `generic-notes` arm (generic + notes) and this variant's own
/// `expert` plan below.
pub const NOTES: &str = include_str!("notes.md");

/// Local copy of `planner::no_config_id`'s exact literal (private there): the
/// "no-config" entry [`planner::scripted_plan_generic`]
/// always inserts into the plan's `configurations` map before calling this
/// arm's [`plan`] — referencing the same literal name resolves against that
/// entry without this function inserting anything itself.
fn no_config_id() -> Result<ConfigId, PlannerError> {
    Ok(ConfigId::new("no-config")?)
}

/// Local copy of `planner::step_expr`'s exact shape (private there): names one
/// gate step.
fn step_expr(
    instance: &str,
    block: polaris_cosmos::contracts::CompositionBlockKey,
    config: ConfigId,
) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Step(polaris_cosmos::composition::StepPlan::new(
        BlockInstanceId::new(instance)?,
        block,
        config,
    )))
}

/// This domain's own self-check-gated shape.
///
/// Reimplements `planner::self_check_gated_expression`'s exact structure
/// (private there) — the same single shared final `conclude` step and single
/// `check-needs-repair` `Choose` the generic code uses — with three substitutions
/// over the generic shape:
///
/// 1. an extra `find-mirrored-tests-0: sphinx.find-mirrored-tests` step,
///    inserted right after `propose-0`;
/// 2. `code.has-check` replaced by `sphinx.has-verification`
///    ([`blocks::has_verification`]);
/// 3. `code.run-check-script` replaced by
///    `sphinx.run-check-and-mirrored-tests`
///    ([`blocks::run_check_and_mirrored_tests`]) at both the first-attempt
///    and repair-round verify positions.
///
/// Everything else — `propose-edit`/`propose-edit-repair` model
/// configuration, `extract`/`apply`/`judge`/`affirm`/`conclude` block
/// choices, the single repair round — is the unmodified generic shape.
fn sphinx_self_check_gated_expression() -> Result<GateExpr, PlannerError> {
    let config = no_config_id()?;
    let model_config = ConfigId::new("propose-edit")?;
    let repair_model_config = ConfigId::new("propose-edit-repair")?;

    let repair_body = GateExpr::Seq(vec![
        step_expr(
            "resume-0",
            repair::resume_model_propose_key(),
            repair_model_config,
        )?,
        step_expr(
            "extract-1",
            repair::extract_patch_from_proposal_key(),
            config.clone(),
        )?,
        step_expr(
            "apply-1",
            configurable::apply_patch_intent_key(),
            config.clone(),
        )?,
        step_expr(
            "verify-1",
            blocks::run_check_and_mirrored_tests_key(),
            config.clone(),
        )?,
        step_expr(
            "judge-test-1",
            repair::judge_test_outcome_key(),
            config.clone(),
        )?,
    ]);
    let outer_choose = GateExpr::Choose {
        predicate: predicates::check_needs_repair_key(),
        when_false: Box::new(step_expr(
            "affirm-1",
            configurable::affirm_assessment_key(),
            config.clone(),
        )?),
        when_true: Box::new(repair_body),
    };
    let with_verification = GateExpr::Seq(vec![
        step_expr(
            "verify-0",
            blocks::run_check_and_mirrored_tests_key(),
            config.clone(),
        )?,
        step_expr(
            "judge-test-0",
            repair::judge_test_outcome_key(),
            config.clone(),
        )?,
        outer_choose,
    ]);
    let without_verification = GateExpr::Seq(vec![step_expr(
        "judge-patch-0",
        repair::judge_patch_intent_key(),
        config.clone(),
    )?]);
    let has_verification_choose = GateExpr::Choose {
        predicate: blocks::has_verification_key(),
        when_true: Box::new(with_verification),
        when_false: Box::new(without_verification),
    };
    Ok(GateExpr::Seq(vec![
        step_expr(
            "propose-0",
            repair::model_propose_conversation_key(),
            model_config,
        )?,
        step_expr(
            "find-mirrored-tests-0",
            blocks::find_mirrored_tests_key(),
            config.clone(),
        )?,
        step_expr(
            "extract-0",
            repair::extract_patch_from_proposal_key(),
            config.clone(),
        )?,
        step_expr(
            "apply-0",
            configurable::apply_patch_intent_key(),
            config.clone(),
        )?,
        has_verification_choose,
        step_expr("conclude", existing::conclude_single_pass_key(), config)?,
    ]))
}

/// This variant's plan for the statement-only edit shape.
///
/// Delegates to [`planner::statement_only_plan_body`] to harvest the exact
/// same model-step configurations the `generic-notes` arm gets, then, only in
/// the one shape the Sphinx research targets (a shell is available but there is
/// no project test runner), discards the generic gate shape and substitutes
/// [`sphinx_self_check_gated_expression`] instead. Every other shape (a test
/// runner, or no shell at all) returns the unmodified generic-plus-notes
/// shape unchanged, exactly like the `generic-notes` arm.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    let (generic_expression, extra) = planner::statement_only_plan_body(facts, attempt, NOTES)?;
    if facts.test_runner_available || !facts.shell_available {
        return Ok((generic_expression, extra));
    }
    Ok((sphinx_self_check_gated_expression()?, extra))
}

/// Extra gate blocks this variant contributes to the sealed coding block
/// catalogue: locating Sphinx's own pre-existing tests for a proposed
/// patch's edited module, deciding whether either that or a submitted check
/// script gives this plan anything to verify with, and running whichever of
/// the two (or both) exist.
pub mod blocks {
    use std::path::Path;

    use polaris::graph::predicate::BoxedPredicate;
    use polaris::graph::{Graph, NodeId};
    use polaris::system::param::{Out, Res};
    use polaris::system::system;
    use polaris::system::system::SystemError;
    use polaris_cosmos::composition::{
        BlockBuildError, BlockFactory, CompositionCatalogue, CompositionCatalogueError, Effect,
        PredicateId, PredicateKey,
    };
    use polaris_cosmos::contracts::{
        CompositionBlockId, CompositionBlockKey, ResolvedConfigurationRef,
    };

    use crate::cosmos::gate_blocks::configurable::ProposedPatchIntent;
    use crate::cosmos::gate_blocks::no_config_schema;
    use crate::cosmos::payload::{AppliedEdit, CheckFailureClass, CodingWorkspace, TestRunOutcome};
    use crate::cosmos::task::ExactEdit;
    use polaris_cosmos::contracts::Status;

    fn exec(error: impl std::fmt::Display) -> SystemError {
        SystemError::ExecutionError(error.to_string())
    }

    fn block_key(id: &str) -> CompositionBlockKey {
        CompositionBlockKey::new(
            CompositionBlockId::new(id).expect("block ID is a valid stable name"),
            crate::cosmos::behavior_version(),
        )
    }

    fn predicate_key(id: &str) -> PredicateKey {
        PredicateKey::new(
            PredicateId::new(id).expect("predicate ID is a valid stable name"),
            crate::cosmos::behavior_version(),
        )
    }

    // -----------------------------------------------------------------------
    // Locating Sphinx's own pre-existing tests for whatever module a
    // proposed patch touched, and passing the proposal's own submitted check
    // through unchanged.
    // -----------------------------------------------------------------------

    /// What [`find_mirrored_tests`] found for one proposed patch: the
    /// pass-through submitted `check` (unchanged from the proposal) plus any
    /// of Sphinx's own pre-existing tests it located on disk.
    ///
    /// Folding the pass-through `check` in here — rather than a separate
    /// type — means [`has_verification`] and [`run_check_and_mirrored_tests`]
    /// each read one upstream type, the same one-type-per-predicate/-block
    /// shape `predicates::has_check`/`repair::RunCheckScriptBlock` already
    /// use.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SphinxVerificationPlan {
        /// The proposal's own submitted check script, unchanged.
        pub check: Option<String>,
        /// Sphinx's own pre-existing test files whose name suggests they
        /// cover the edited module, deduplicated and capped at
        /// [`MAX_MIRRORED_TEST_PATHS`], in deterministic (sorted) order.
        pub mirrored_test_paths: Vec<String>,
    }

    /// Bound on how many mirrored test paths [`find_mirrored_tests`]
    /// returns — the same "bounded, deterministic, no surprise growth"
    /// discipline other survey-style blocks in this catalogue already apply
    /// to their own site lists.
    const MAX_MIRRORED_TEST_PATHS: usize = 4;

    /// Sphinx's real layout mirrors `sphinx/<pkg>/<module>.py` with
    /// `tests/test_<pkg>_<module>.py` (e.g. `sphinx/ext/napoleon/docstring.py`
    /// mirrors `tests/test_ext_napoleon_docstring.py`) — a real, if fuzzy,
    /// project convention, not a per-task heuristic.
    fn mirrored_candidate_path(edited_file: &str) -> Option<String> {
        let relative = edited_file.strip_prefix("sphinx/")?.strip_suffix(".py")?;
        Some(format!("tests/test_{}.py", relative.replace('/', "_")))
    }

    fn edited_file_stem(edited_file: &str) -> Option<&str> {
        edited_file
            .rsplit('/')
            .next()
            .and_then(|name| name.strip_suffix(".py"))
    }

    fn list_top_level_test_files(working_dir: &Path) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(working_dir.join("tests")) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".py"))
            .collect()
    }

    fn find_mirrored_test_paths(workspace: &CodingWorkspace, edits: &[ExactEdit]) -> Vec<String> {
        let mut found = std::collections::BTreeSet::new();
        let mut top_level: Option<Vec<String>> = None;
        for edit in edits {
            let file = edit.file();
            if !file.starts_with("sphinx/") || !file.ends_with(".py") {
                continue;
            }
            if let Some(candidate) = mirrored_candidate_path(file)
                && workspace.read(&candidate).is_ok()
            {
                found.insert(candidate);
                continue;
            }
            let Some(stem) = edited_file_stem(file) else {
                continue;
            };
            let listing = top_level
                .get_or_insert_with(|| list_top_level_test_files(&workspace.config().working_dir));
            for name in listing.iter() {
                if name.contains(stem) {
                    found.insert(format!("tests/{name}"));
                }
            }
        }
        found.into_iter().take(MAX_MIRRORED_TEST_PATHS).collect()
    }

    #[system]
    async fn find_mirrored_tests(
        workspace: Res<CodingWorkspace>,
        proposal: Out<ProposedPatchIntent>,
    ) -> SphinxVerificationPlan {
        SphinxVerificationPlan {
            check: proposal.patch.check.clone(),
            mirrored_test_paths: find_mirrored_test_paths(&workspace, &proposal.patch.edits),
        }
    }

    /// Locates Sphinx's own pre-existing test file(s) for whatever module a
    /// proposed patch touched, and passes the proposal's own submitted
    /// `check` through unchanged.
    ///
    /// Wraps [`find_mirrored_tests`]: `CodingWorkspace,
    /// Out<ProposedPatchIntent> -> SphinxVerificationPlan`. Read-only: pure
    /// filesystem lookup, no shell, no model, no mutation, never fails (an
    /// unreadable or absent `tests/` directory yields an empty
    /// `mirrored_test_paths`, not an error).
    pub struct FindMirroredTestsBlock;

    impl BlockFactory for FindMirroredTestsBlock {
        fn build(
            &self,
            graph: &mut Graph,
            _config: &ResolvedConfigurationRef,
        ) -> Result<NodeId, BlockBuildError> {
            Ok(graph.add_system_node(find_mirrored_tests))
        }
    }

    /// Returns [`FindMirroredTestsBlock`]'s registered key.
    #[must_use]
    pub fn find_mirrored_tests_key() -> CompositionBlockKey {
        block_key("sphinx.find-mirrored-tests")
    }

    // -----------------------------------------------------------------------
    // Whether there is anything to verify with at all.
    // -----------------------------------------------------------------------

    /// Whether either signal [`find_mirrored_tests`] could have found
    /// exists: a located mirrored test, or a non-blank submitted check.
    ///
    /// Replaces `predicates::has_check` as this domain's self-check gate's
    /// own condition (it does not sit alongside it): a plain submitted check
    /// is not the only way a Sphinx patch can be verified.
    #[must_use]
    pub fn has_verification(plan: &SphinxVerificationPlan) -> bool {
        !plan.mirrored_test_paths.is_empty()
            || plan
                .check
                .as_ref()
                .is_some_and(|check| !check.trim().is_empty())
    }

    /// Returns [`has_verification`]'s registered key.
    #[must_use]
    pub fn has_verification_key() -> PredicateKey {
        predicate_key("sphinx.has-verification")
    }

    // -----------------------------------------------------------------------
    // Running the submitted check script and/or the mirrored tests.
    // -----------------------------------------------------------------------

    /// Fixed, host-controlled relative path this block writes a submitted
    /// `check` script to before running it. A local literal duplicate of
    /// `gate_blocks::repair::CHECK_SCRIPT_PATH` (`pub(super)`-scoped there,
    /// unreachable from this module — see the module-level doc comment);
    /// reusing the identical string is safe because this block replaces
    /// `repair::RunCheckScriptBlock` in this domain's plan shape rather than
    /// running alongside it, so the two never write to the same workspace
    /// concurrently.
    const CHECK_SCRIPT_PATH: &str = "check_script.py";

    /// Output cap per stream. Matches
    /// `gate_blocks::configurable::MAX_PYTHON_OUTPUT_BYTES` (`pub(super)`,
    /// unreachable from this module).
    const MAX_OUTPUT_BYTES: usize = 4_096;

    /// Timeout for the submitted check script. Matches
    /// `gate_blocks::configurable::PYTHON_SCRIPT_TIMEOUT_SECS`.
    const CHECK_TIMEOUT_SECS: u64 = 30;

    /// Timeout for the mirrored pytest run: longer than the check script's,
    /// since it may run more than one test file.
    const MIRRORED_TEST_TIMEOUT_SECS: u64 = 60;

    fn cap(text: &str) -> String {
        if text.len() <= MAX_OUTPUT_BYTES {
            text.to_owned()
        } else {
            let mut capped = text[..MAX_OUTPUT_BYTES].to_owned();
            capped.push_str("\n… truncated");
            capped
        }
    }

    fn render_shell_result(result: &polaris::shell::ExecutionResult, timeout_secs: u64) -> String {
        let mut rendered = match result.exit_code {
            Some(code) => format!("exit code: {code}"),
            None => "exit code: (none; the process was killed)".to_owned(),
        };
        if result.timed_out {
            rendered.push_str(&format!("\ntimed out after {timeout_secs}s"));
        }
        if !result.stdout.is_empty() {
            rendered.push_str(&format!("\nstdout:\n{}", cap(&result.stdout)));
        }
        if !result.stderr.is_empty() {
            rendered.push_str(&format!("\nstderr:\n{}", cap(&result.stderr)));
        }
        rendered
    }

    struct ShellRunOutcome {
        success: bool,
        rendered: String,
        exit_code: Option<i32>,
        timed_out: bool,
        stderr: String,
    }

    /// Local reimplementation of
    /// `gate_blocks::configurable::run_python_script`'s permission-check /
    /// execute / render shape (`pub(super)`-scoped there, unreachable from
    /// this module — see the module-level doc comment): runs one shell
    /// command with a fixed timeout, refusing rather than running when the
    /// policy would require human confirmation.
    ///
    /// `command` must keep this domain's unattended grant's own literal
    /// prefix (`python `): the allow-list is a glob match on the whole
    /// command string (`polaris_shell::executor::wildcard_match`), not a
    /// resolved-executable comparison, so `/opt/.../python ...` does not
    /// match a `python *` grant the same call with a bare `python ...` does.
    /// This is why [`write_and_run_check_script`]/[`run_mirrored_tests`]
    /// resolve a different interpreter from *inside* the script they hand to
    /// this literal `python`, rather than naming one here.
    async fn run_shell_capped(
        shell: &polaris::shell::ShellExecutor,
        working_dir: &Path,
        command: String,
        timeout_secs: u64,
    ) -> Result<ShellRunOutcome, String> {
        if !matches!(
            shell.check_permission(&command),
            polaris::shell::ShellPermission::Allow
        ) {
            return Err(format!(
                "the shell policy does not allow '{command}' to run unattended"
            ));
        }
        let request = polaris::shell::ShellRequest::new(command.clone())
            .with_working_dir(working_dir.to_path_buf())
            .with_timeout(timeout_secs);
        let response = shell
            .execute(request)
            .await
            .map_err(|error| format!("could not run '{command}': {error}"))?;
        match response {
            polaris::shell::ShellResponse::Executed(result) => Ok(ShellRunOutcome {
                success: result.exit_code == Some(0) && !result.timed_out,
                rendered: render_shell_result(&result, timeout_secs),
                exit_code: result.exit_code,
                timed_out: result.timed_out,
                stderr: result.stderr,
            }),
            polaris::shell::ShellResponse::ConfirmationRequired(_) => Err(format!(
                "the shell policy would require confirmation to run '{command}'; refusing \
                 rather than blocking on a human that is not there"
            )),
        }
    }

    fn normalize_check_script(script: &str) -> String {
        if !script.contains('\n') && script.contains("\\n") {
            script.replace("\\n", "\n")
        } else {
            script.to_owned()
        }
    }

    /// Interpreters this preamble tries when the ambient `python` the shell
    /// command names cannot import Sphinx: a common containerized dev-image
    /// layout, where the project and its dependencies live in a conda
    /// environment named `testbed` rather than on the system interpreter's
    /// own path. A host whose ambient `python` already has Sphinx installed,
    /// or one that names its environment differently, matches none of these
    /// and runs exactly as before — this is a fallback, not a requirement.
    const TESTBED_PYTHON_CANDIDATES: &[&str] = &[
        "/opt/miniconda3/envs/testbed/bin/python",
        "/opt/conda/envs/testbed/bin/python",
        "/root/miniconda3/envs/testbed/bin/python",
    ];

    /// A Python source preamble that re-execs the running interpreter into
    /// the first candidate in `candidates` that both exists and can import
    /// `marker_module`, when the interpreter that started the process
    /// cannot. Prepended to every script this block writes and runs, so a
    /// verification command whose literal text must stay `python ...` (see
    /// [`run_shell_capped`]'s doc comment) still runs Sphinx's own test
    /// dependencies rather than whatever the bare `python` on `PATH`
    /// resolves to.
    ///
    /// `os.execv` replaces the process image in place — same pid, same
    /// stdio — so the caller's timeout and output capture see it as one
    /// continuous run either way.
    fn reexec_preamble(marker_module: &str, candidates: &[&str]) -> String {
        let candidate_list = candidates
            .iter()
            .map(|candidate| format!("{candidate:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "import sys as _sx_sys, os as _sx_os, importlib.util as _sx_ilu\n\
             if _sx_ilu.find_spec({marker_module:?}) is None:\n\
             \u{20}   for _sx_candidate in [{candidate_list}]:\n\
             \u{20}       if _sx_os.path.exists(_sx_candidate) and _sx_os.path.realpath(\
             _sx_candidate) != _sx_os.path.realpath(_sx_sys.executable):\n\
             \u{20}           _sx_os.execv(_sx_candidate, [_sx_candidate] + _sx_sys.argv)\n"
        )
    }

    /// [`reexec_preamble`] with this domain's own marker (`sphinx` itself —
    /// every check or mirrored test this block runs is for a Sphinx issue,
    /// so nothing it runs can mean anything without it) and candidate list.
    fn sphinx_reexec_preamble() -> String {
        reexec_preamble("sphinx", TESTBED_PYTHON_CANDIDATES)
    }

    async fn write_python_file(
        working_dir: &Path,
        filename: &str,
        content: &str,
    ) -> Result<std::path::PathBuf, String> {
        let dir = crate::state::state_dir(working_dir)
            .map_err(|error| format!("could not prepare the state directory: {error}"))?;
        tokio::fs::write(dir.join(filename), content)
            .await
            .map_err(|error| format!("could not write {filename}: {error}"))?;
        Ok(Path::new(crate::state::STATE_DIR_NAME).join(filename))
    }

    async fn write_and_run_check_script(
        shell: &polaris::shell::ShellExecutor,
        working_dir: &Path,
        script: &str,
    ) -> Result<ShellRunOutcome, String> {
        let content = format!(
            "{}\n{}",
            sphinx_reexec_preamble(),
            normalize_check_script(script)
        );
        let relative = write_python_file(working_dir, CHECK_SCRIPT_PATH, &content).await?;
        run_shell_capped(
            shell,
            working_dir,
            format!("python {}", relative.display()),
            CHECK_TIMEOUT_SECS,
        )
        .await
    }

    fn shell_quote(path: &str) -> String {
        format!("'{}'", path.replace('\'', "'\\''"))
    }

    /// Fixed relative path this block writes its own pytest-launching
    /// wrapper to (see [`run_mirrored_tests`]) — a sibling of
    /// [`CHECK_SCRIPT_PATH`], distinct so a turn that runs both a submitted
    /// check and mirrored tests never has one overwrite the other on disk.
    const MIRRORED_TEST_RUNNER_PATH: &str = "run_mirrored_tests.py";

    async fn run_mirrored_tests(
        shell: &polaris::shell::ShellExecutor,
        working_dir: &Path,
        paths: &[String],
    ) -> Result<ShellRunOutcome, String> {
        let quoted = paths
            .iter()
            .map(|path| shell_quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        // A wrapper script, not a literal `python -m pytest`: the re-exec
        // preamble above must run before pytest itself is imported, and
        // `python -m pytest` gives no way to run code first.
        let content = format!(
            "{}\nimport sys\nimport pytest\n\nsys.exit(pytest.main(sys.argv[1:]))\n",
            sphinx_reexec_preamble()
        );
        let relative = write_python_file(working_dir, MIRRORED_TEST_RUNNER_PATH, &content).await?;
        run_shell_capped(
            shell,
            working_dir,
            format!("python {} {quoted}", relative.display()),
            MIRRORED_TEST_TIMEOUT_SECS,
        )
        .await
    }

    /// Local reimplementation of
    /// `gate_blocks::repair::classify_check_failure`'s exact heuristic
    /// (private there, and its parameter type is `pub(super)`-scoped either
    /// way — see the module-level doc comment), over raw fields instead of
    /// `configurable::PythonRunOutcome`. Any change to the original must be
    /// mirrored here by hand.
    fn classify_shell_failure(
        timed_out: bool,
        exit_code: Option<i32>,
        stderr: &str,
    ) -> CheckFailureClass {
        if timed_out || matches!(exit_code, None | Some(126 | 127)) {
            return CheckFailureClass::Infrastructure;
        }
        let last_exception = stderr
            .lines()
            .rev()
            .find(|line| line.trim().contains("Error:"))
            .unwrap_or("")
            .trim();
        if last_exception.starts_with("ModuleNotFoundError:")
            || last_exception.starts_with("ImportError:")
            || ((last_exception.starts_with("SyntaxError:")
                || last_exception.starts_with("IndentationError:"))
                && stderr.contains(CHECK_SCRIPT_PATH))
            || stderr.contains("Failed building wheel")
            || stderr.contains("error: subprocess-exited-with-error")
            || stderr.contains("compilation terminated")
            || (stderr.contains("error: command '") && stderr.contains("failed"))
        {
            CheckFailureClass::Infrastructure
        } else {
            CheckFailureClass::Behavior
        }
    }

    /// A real behavior failure must never be masked by an unrelated
    /// infrastructure hiccup in the *other* stream: if either the check or
    /// the mirrored tests failed for a behavior reason, the combined verdict
    /// is [`CheckFailureClass::Behavior`], so the repair loop still fires.
    fn combine_failure_class(
        current: Option<CheckFailureClass>,
        new: CheckFailureClass,
    ) -> CheckFailureClass {
        if current == Some(CheckFailureClass::Behavior) || new == CheckFailureClass::Behavior {
            CheckFailureClass::Behavior
        } else {
            CheckFailureClass::Infrastructure
        }
    }

    #[system]
    async fn run_check_and_mirrored_tests(
        workspace: Res<CodingWorkspace>,
        shell: Res<polaris::shell::ShellExecutor>,
        plan: Out<SphinxVerificationPlan>,
        _applied: Out<AppliedEdit>,
    ) -> Result<TestRunOutcome, SystemError> {
        let working_dir = workspace.config().working_dir.as_path();
        let mut sections = Vec::new();
        let mut overall_success = true;
        let mut failure_class: Option<CheckFailureClass> = None;
        let mut ran_anything = false;

        let check = plan
            .check
            .as_deref()
            .filter(|check| !check.trim().is_empty());
        if let Some(check) = check {
            ran_anything = true;
            match write_and_run_check_script(&shell, working_dir, check).await {
                Ok(outcome) => {
                    if !outcome.success {
                        overall_success = false;
                        let class = classify_shell_failure(
                            outcome.timed_out,
                            outcome.exit_code,
                            &outcome.stderr,
                        );
                        failure_class = Some(combine_failure_class(failure_class, class));
                    }
                    sections.push(format!("check script:\n{}", outcome.rendered));
                }
                Err(message) => {
                    overall_success = false;
                    failure_class = Some(combine_failure_class(
                        failure_class,
                        CheckFailureClass::Infrastructure,
                    ));
                    sections.push(format!("check script:\n{message}"));
                }
            }
        }

        if !plan.mirrored_test_paths.is_empty() {
            ran_anything = true;
            match run_mirrored_tests(&shell, working_dir, &plan.mirrored_test_paths).await {
                Ok(outcome) => {
                    if !outcome.success {
                        overall_success = false;
                        let class = classify_shell_failure(
                            outcome.timed_out,
                            outcome.exit_code,
                            &outcome.stderr,
                        );
                        failure_class = Some(combine_failure_class(failure_class, class));
                    }
                    sections.push(format!("mirrored tests:\n{}", outcome.rendered));
                }
                Err(message) => {
                    overall_success = false;
                    failure_class = Some(combine_failure_class(
                        failure_class,
                        CheckFailureClass::Infrastructure,
                    ));
                    sections.push(format!("mirrored tests:\n{message}"));
                }
            }
        }

        if !ran_anything {
            return Err(exec(
                "a run-check-and-mirrored-tests block ran with neither a check nor a mirrored \
                 test; the has-verification gate should have routed elsewhere",
            ));
        }

        Ok(TestRunOutcome {
            status: if overall_success {
                Status::Pass
            } else {
                Status::Issue
            },
            output: sections.join("\n\n"),
            check_failure: failure_class,
        })
    }

    /// Runs a proposal's submitted check script and/or Sphinx's own mirrored
    /// tests (whichever [`SphinxVerificationPlan`] found), keeping a bounded
    /// tail of the combined output.
    ///
    /// Wraps [`run_check_and_mirrored_tests`]: `CodingWorkspace,
    /// polaris::shell::ShellExecutor, Out<SphinxVerificationPlan>,
    /// Out<AppliedEdit> -> TestRunOutcome`. Read-only. Replaces
    /// `repair::RunCheckScriptBlock` in this domain's plan shape (never runs
    /// alongside it). Hard-errors only if neither a check nor a mirrored
    /// test path is present — the same defensive shape
    /// `repair::run_check_script` uses for a missing check, trusting its own
    /// routing predicate ([`has_verification`]) to prevent that call from
    /// happening in practice.
    pub struct RunCheckAndMirroredTestsBlock;

    impl BlockFactory for RunCheckAndMirroredTestsBlock {
        fn build(
            &self,
            graph: &mut Graph,
            _config: &ResolvedConfigurationRef,
        ) -> Result<NodeId, BlockBuildError> {
            Ok(graph.add_system_node(run_check_and_mirrored_tests))
        }
    }

    /// Returns [`RunCheckAndMirroredTestsBlock`]'s registered key.
    #[must_use]
    pub fn run_check_and_mirrored_tests_key() -> CompositionBlockKey {
        block_key("sphinx.run-check-and-mirrored-tests")
    }

    /// This variant's own block keys, folded into the sealed catalogue's key
    /// listing test.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        vec![
            find_mirrored_tests_key(),
            run_check_and_mirrored_tests_key(),
        ]
    }

    /// Registers this variant's own blocks and predicate into the sealed
    /// catalogue.
    ///
    /// # Errors
    /// Returns [`CompositionCatalogueError::DuplicateBlock`] or
    /// `DuplicatePredicate` if a later addition here names a key already
    /// registered elsewhere.
    pub fn register_all(
        catalogue: &mut CompositionCatalogue,
    ) -> Result<(), CompositionCatalogueError> {
        let schema = no_config_schema();
        catalogue.register_block(
            find_mirrored_tests_key(),
            std::sync::Arc::new(FindMirroredTestsBlock),
            schema.clone(),
            Effect::ReadOnly,
        )?;
        catalogue.register_block(
            run_check_and_mirrored_tests_key(),
            std::sync::Arc::new(RunCheckAndMirroredTestsBlock),
            schema,
            Effect::ReadOnly,
        )?;
        catalogue.register_predicate(
            has_verification_key(),
            BoxedPredicate::new::<SphinxVerificationPlan>(has_verification),
        )?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use polaris::graph::GraphExecutor;
        use polaris::system::param::SystemContext;

        use super::*;
        use crate::cosmos::gate_blocks::configurable::{ModelConversation, PatchIntent};
        use crate::cosmos::task::{CodingIntent, CodingTask, SuccessCheck};

        fn no_config() -> ResolvedConfigurationRef {
            ResolvedConfigurationRef::inline(
                no_config_schema(),
                polaris_cosmos::contracts::ConfigurationValue::Null,
            )
            .expect("a null value satisfies the no-config schema")
        }

        fn workspace_with(dir: &std::path::Path) -> CodingWorkspace {
            let task = CodingTask::from_statement(
                "sphinx-expert-block-fixture",
                dir,
                "fix the bug",
                CodingIntent::Careful,
                SuccessCheck::stated_edits(),
            )
            .expect("well-formed task");
            CodingWorkspace::for_task(&task)
        }

        // -------------------------------------------------------------
        // find_mirrored_tests
        // -------------------------------------------------------------

        #[tokio::test]
        async fn find_mirrored_tests_matches_the_conventional_path() {
            let dir = tempfile::tempdir().expect("temp project");
            std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
            std::fs::write(
                dir.path().join("tests/test_ext_napoleon_docstring.py"),
                "def test_it(): pass\n",
            )
            .expect("seed mirrored test");
            let workspace = workspace_with(dir.path());

            let mut graph = Graph::new();
            graph.add_system_node(|| async {
                ProposedPatchIntent {
                    patch: PatchIntent {
                        edits: vec![
                            ExactEdit::new("sphinx/ext/napoleon/docstring.py", "a", "b")
                                .expect("well-formed edit"),
                        ],
                        check: Some("print('ok')".to_owned()),
                    },
                    conversation: ModelConversation::default(),
                }
            });
            FindMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

            let mut context = SystemContext::new();
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("find-mirrored-tests runs");
            let plan = execution
                .output::<SphinxVerificationPlan>()
                .expect("produces a SphinxVerificationPlan");
            assert_eq!(
                plan.mirrored_test_paths,
                vec!["tests/test_ext_napoleon_docstring.py".to_owned()]
            );
            assert_eq!(plan.check.as_deref(), Some("print('ok')"));
        }

        #[tokio::test]
        async fn find_mirrored_tests_falls_back_to_a_fuzzy_stem_match() {
            let dir = tempfile::tempdir().expect("temp project");
            std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
            // Not the conventional `test_<pkg>_<module>.py` name, but it
            // still contains the edited module's own stem.
            std::fs::write(
                dir.path().join("tests/test_docstring_variants.py"),
                "def test_it(): pass\n",
            )
            .expect("seed fuzzy-matched test");
            let workspace = workspace_with(dir.path());

            let mut graph = Graph::new();
            graph.add_system_node(|| async {
                ProposedPatchIntent {
                    patch: PatchIntent {
                        edits: vec![
                            ExactEdit::new("sphinx/ext/napoleon/docstring.py", "a", "b")
                                .expect("well-formed edit"),
                        ],
                        check: None,
                    },
                    conversation: ModelConversation::default(),
                }
            });
            FindMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut context = SystemContext::new();
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("find-mirrored-tests runs");
            let plan = execution
                .output::<SphinxVerificationPlan>()
                .expect("produces a SphinxVerificationPlan");
            assert_eq!(
                plan.mirrored_test_paths,
                vec!["tests/test_docstring_variants.py".to_owned()]
            );
        }

        #[tokio::test]
        async fn find_mirrored_tests_reports_nothing_for_an_untested_module() {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());

            let mut graph = Graph::new();
            graph.add_system_node(|| async {
                ProposedPatchIntent {
                    patch: PatchIntent {
                        edits: vec![
                            ExactEdit::new("sphinx/ext/napoleon/docstring.py", "a", "b")
                                .expect("well-formed edit"),
                        ],
                        check: None,
                    },
                    conversation: ModelConversation::default(),
                }
            });
            FindMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut context = SystemContext::new();
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("find-mirrored-tests runs even with no tests directory");
            let plan = execution
                .output::<SphinxVerificationPlan>()
                .expect("produces a SphinxVerificationPlan");
            assert!(plan.mirrored_test_paths.is_empty());
            assert!(plan.check.is_none());
        }

        #[tokio::test]
        async fn find_mirrored_tests_caps_at_the_declared_bound() {
            let dir = tempfile::tempdir().expect("temp project");
            std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
            let workspace = workspace_with(dir.path());

            let mut edits = Vec::new();
            for index in 0..(MAX_MIRRORED_TEST_PATHS + 2) {
                let module = format!("sphinx/ext/mod{index}.py");
                std::fs::write(
                    dir.path().join(format!("tests/test_ext_mod{index}.py")),
                    "def test_it(): pass\n",
                )
                .expect("seed mirrored test");
                edits.push(ExactEdit::new(module, "a", "b").expect("well-formed edit"));
            }

            let mut graph = Graph::new();
            graph.add_system_node(move || {
                let edits = edits.clone();
                async move {
                    ProposedPatchIntent {
                        patch: PatchIntent { edits, check: None },
                        conversation: ModelConversation::default(),
                    }
                }
            });
            FindMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut context = SystemContext::new();
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("find-mirrored-tests runs");
            let plan = execution
                .output::<SphinxVerificationPlan>()
                .expect("produces a SphinxVerificationPlan");
            assert_eq!(plan.mirrored_test_paths.len(), MAX_MIRRORED_TEST_PATHS);
        }

        // -------------------------------------------------------------
        // has_verification
        // -------------------------------------------------------------

        #[test]
        fn has_verification_is_true_with_a_mirrored_test() {
            assert!(has_verification(&SphinxVerificationPlan {
                check: None,
                mirrored_test_paths: vec!["tests/test_x.py".to_owned()],
            }));
        }

        #[test]
        fn has_verification_is_true_with_a_nonblank_check() {
            assert!(has_verification(&SphinxVerificationPlan {
                check: Some("print('ok')".to_owned()),
                mirrored_test_paths: Vec::new(),
            }));
        }

        #[test]
        fn has_verification_is_false_with_neither() {
            assert!(!has_verification(&SphinxVerificationPlan {
                check: Some("   ".to_owned()),
                mirrored_test_paths: Vec::new(),
            }));
            assert!(!has_verification(&SphinxVerificationPlan {
                check: None,
                mirrored_test_paths: Vec::new(),
            }));
        }

        // -------------------------------------------------------------
        // reexec_preamble
        //
        // These run the generated preamble as real Python via `python3`
        // directly (bypassing `ShellExecutor`, already covered by the
        // shell-permission tests below) to prove the re-exec mechanism
        // itself — not just its generated source text — behaves as
        // designed: a `python3` wrapper script standing in for a "second
        // interpreter" that can see a marker module the first cannot.
        // -------------------------------------------------------------

        fn run_python_source(source: &str) -> std::process::Output {
            let dir = tempfile::tempdir().expect("temp dir");
            let script = dir.path().join("script.py");
            std::fs::write(&script, source).expect("write script");
            std::process::Command::new("python3")
                .arg(&script)
                .output()
                .expect("run python3")
        }

        /// Builds a `python3` stand-in at `<dir>/candidate_python` that runs
        /// real `python3` with `fake_site` prepended to `PYTHONPATH`, so a
        /// marker module placed only under `fake_site` is importable
        /// through this candidate but not through a plain `python3`.
        fn candidate_interpreter_with_site(
            dir: &std::path::Path,
            fake_site: &std::path::Path,
        ) -> std::path::PathBuf {
            use std::os::unix::fs::PermissionsExt;

            let candidate = dir.join("candidate_python");
            std::fs::write(
                &candidate,
                format!(
                    "#!/bin/sh\nexec env PYTHONPATH=\"{}\" python3 \"$@\"\n",
                    fake_site.display()
                ),
            )
            .expect("write candidate interpreter");
            let mut perms = std::fs::metadata(&candidate)
                .expect("stat candidate")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&candidate, perms).expect("chmod candidate");
            candidate
        }

        #[test]
        fn reexec_preamble_switches_into_a_candidate_that_has_the_marker_module() {
            let dir = tempfile::tempdir().expect("temp dir");
            let fake_site = dir.path().join("fake_site");
            std::fs::create_dir_all(fake_site.join("sx_test_marker_mod"))
                .expect("create fake package dir");
            std::fs::write(fake_site.join("sx_test_marker_mod").join("__init__.py"), "")
                .expect("write fake package");
            let candidate = candidate_interpreter_with_site(dir.path(), &fake_site);

            let preamble = reexec_preamble(
                "sx_test_marker_mod",
                &[candidate.to_str().expect("utf8 path")],
            );
            let source = format!("{preamble}\nimport sx_test_marker_mod\nprint('marker ok')\n");
            let output = run_python_source(&source);
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("marker ok"));
        }

        #[test]
        fn reexec_preamble_is_a_no_op_when_the_marker_module_is_already_importable() {
            // `sys` is always importable, so the guard must short-circuit
            // before ever touching the (nonexistent) candidate.
            let preamble = reexec_preamble("sys", &["/definitely/not/a/real/interpreter"]);
            let source = format!("{preamble}\nprint('no reexec needed')\n");
            let output = run_python_source(&source);
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains("no reexec needed"));
        }

        #[test]
        fn reexec_preamble_is_a_no_op_when_no_candidate_exists() {
            let preamble = reexec_preamble(
                "sx_test_marker_mod_missing_everywhere",
                &["/definitely/not/a/real/interpreter"],
            );
            let source = format!("{preamble}\nprint('fell through safely')\n");
            let output = run_python_source(&source);
            assert!(
                output.status.success(),
                "a preamble with no usable candidate must fall through, not error: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("fell through safely"));
        }

        // -------------------------------------------------------------
        // run_check_and_mirrored_tests
        // -------------------------------------------------------------

        /// A minimal stand-in `pytest` collector: imports each given file and
        /// calls every top-level `test_*` callable, exiting non-zero if any
        /// raised. The shim fakes just enough of `python -m pytest <files>`'s
        /// observable behaviour (exit code, an `AssertionError`'s message on
        /// stderr) to exercise the real command-construction and
        /// output-capture path in [`run_mirrored_tests`] against a real
        /// subprocess without needing pytest on the test host.
        const FAKE_PYTEST_COLLECTOR: &str = r#"
import sys
import traceback

failures = 0
for path in sys.argv[1:]:
    namespace = {}
    try:
        with open(path) as handle:
            source = handle.read()
        exec(compile(source, path, "exec"), namespace)
    except Exception:
        traceback.print_exc()
        failures += 1
        continue
    for name, value in list(namespace.items()):
        if name.startswith("test_") and callable(value):
            try:
                value()
            except Exception:
                traceback.print_exc()
                failures += 1
sys.exit(1 if failures else 0)
"#;

        fn python_shim_shell(
            working_dir: &std::path::Path,
        ) -> (tempfile::TempDir, polaris::shell::ShellExecutor) {
            use std::os::unix::fs::PermissionsExt;

            let bin_dir = tempfile::tempdir().expect("temp bin dir");
            std::fs::write(bin_dir.path().join("fake_pytest.py"), FAKE_PYTEST_COLLECTOR)
                .expect("write fake pytest collector");
            let python_shim = bin_dir.path().join("python");
            std::fs::write(
                &python_shim,
                format!(
                    "#!/bin/sh\n\
                     case \"$1\" in\n\
                     \u{20}   *{MIRRORED_TEST_RUNNER_PATH})\n\
                     \u{20}       shift 1\n\
                     \u{20}       exec python3 \"$(dirname \"$0\")/fake_pytest.py\" \"$@\"\n\
                     \u{20}       ;;\n\
                     esac\n\
                     exec python3 \"$@\"\n"
                ),
            )
            .expect("write shim");
            let mut perms = std::fs::metadata(&python_shim)
                .expect("stat shim")
                .permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&python_shim, perms).expect("chmod shim");

            let shell = polaris::shell::ShellExecutor::new(
                polaris::shell::ShellConfig::new()
                    .with_working_dir(working_dir)
                    .with_allowed_dirs(vec![working_dir.to_path_buf()])
                    .with_allowed_commands(vec!["python *".to_owned()])
                    .with_path(format!("{}:/usr/bin:/bin", bin_dir.path().display())),
            );
            (bin_dir, shell)
        }

        fn seed_graph(graph: &mut Graph, plan: SphinxVerificationPlan) {
            graph.add_system_node(move || {
                let plan = plan.clone();
                async move { plan }
            });
            graph.add_system_node(|| async {
                AppliedEdit {
                    outcomes: Vec::new(),
                }
            });
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_passes_when_the_check_exits_zero() {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let (_bin_dir, shell) = python_shim_shell(&working_dir);

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: Some("print('ok')".to_owned()),
                    mirrored_test_paths: Vec::new(),
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.output);
            assert_eq!(outcome.check_failure, None);
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_passes_when_only_mirrored_tests_pass() {
            let dir = tempfile::tempdir().expect("temp project");
            std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
            std::fs::write(
                dir.path().join("tests/test_ok.py"),
                "def test_ok():\n    assert True\n",
            )
            .expect("seed passing test");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let (_bin_dir, shell) = python_shim_shell(&working_dir);

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: None,
                    mirrored_test_paths: vec!["tests/test_ok.py".to_owned()],
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Pass, "{:?}", outcome.output);
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_reports_behavior_failure_from_the_check() {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let (_bin_dir, shell) = python_shim_shell(&working_dir);

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: Some("assert False, 'wrong result'".to_owned()),
                    mirrored_test_paths: Vec::new(),
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Issue);
            assert_eq!(outcome.check_failure, Some(CheckFailureClass::Behavior));
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_reports_infrastructure_failure_from_the_check() {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let (_bin_dir, shell) = python_shim_shell(&working_dir);

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: Some("import nonexistent_module_xyz".to_owned()),
                    mirrored_test_paths: Vec::new(),
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Issue);
            assert_eq!(
                outcome.check_failure,
                Some(CheckFailureClass::Infrastructure)
            );
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_prefers_behavior_when_both_streams_fail_differently()
        {
            let dir = tempfile::tempdir().expect("temp project");
            std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
            std::fs::write(
                dir.path().join("tests/test_bad.py"),
                "def test_bad():\n    assert False, 'wrong result'\n",
            )
            .expect("seed failing test");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let (_bin_dir, shell) = python_shim_shell(&working_dir);

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: Some("import nonexistent_module_xyz".to_owned()),
                    mirrored_test_paths: vec!["tests/test_bad.py".to_owned()],
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Issue);
            assert_eq!(
                outcome.check_failure,
                Some(CheckFailureClass::Behavior),
                "a real behavior failure in either stream must not be masked by an \
                 infrastructure failure in the other: {:?}",
                outcome.output
            );
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_reports_issue_when_the_shell_policy_denies_the_script()
         {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            // No allowed-command pattern at all, so `python ...` defaults to
            // `ShellPermission::Confirm`, not `Allow`.
            let shell = polaris::shell::ShellExecutor::new(
                polaris::shell::ShellConfig::new().with_working_dir(&working_dir),
            );

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: Some("print('ok')".to_owned()),
                    mirrored_test_paths: Vec::new(),
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let execution = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await
                .expect("run-check-and-mirrored-tests runs even when the shell policy denies it");
            let outcome = execution
                .output::<TestRunOutcome>()
                .expect("produces a TestRunOutcome");
            assert_eq!(outcome.status, Status::Issue);
            assert!(
                outcome.output.contains("does not allow"),
                "the denial reason must be captured as output: {:?}",
                outcome.output
            );
            assert_eq!(
                outcome.check_failure,
                Some(CheckFailureClass::Infrastructure)
            );
        }

        #[tokio::test]
        async fn run_check_and_mirrored_tests_fails_hard_with_neither_check_nor_mirrored_tests() {
            let dir = tempfile::tempdir().expect("temp project");
            let workspace = workspace_with(dir.path());
            let working_dir = workspace.config().working_dir.clone();
            let shell = polaris::shell::ShellExecutor::new(
                polaris::shell::ShellConfig::new().with_working_dir(&working_dir),
            );

            let mut graph = Graph::new();
            seed_graph(
                &mut graph,
                SphinxVerificationPlan {
                    check: None,
                    mirrored_test_paths: Vec::new(),
                },
            );
            RunCheckAndMirroredTestsBlock
                .build(&mut graph, &no_config())
                .expect("builds");

            let mut globals = polaris::system::resource::Resources::new();
            globals.insert(shell);
            let mut context = SystemContext::with_globals(Arc::new(globals));
            context.insert(workspace);
            let result = GraphExecutor::new()
                .execute(&graph, &mut context, None, None)
                .await;
            assert!(
                result.is_err(),
                "a run-check-and-mirrored-tests block with neither a check nor a mirrored \
                 test must not silently succeed"
            );
        }

        #[test]
        fn every_key_is_distinct() {
            let mut keys = keys();
            let before = keys.len();
            keys.sort();
            keys.dedup();
            assert_eq!(keys.len(), before);
        }

        #[test]
        fn blocks_and_predicate_register_into_the_real_catalogue() {
            let catalogue = crate::cosmos::gate_blocks::coding_block_catalogue();
            assert!(catalogue.block(&find_mirrored_tests_key()).is_some());
            assert!(
                catalogue
                    .block(&run_check_and_mirrored_tests_key())
                    .is_some()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::{CompositionLimits, materialize};
    use polaris_cosmos::topology::TopologyContractKey;

    use super::*;
    use crate::cosmos::gate_blocks::coding_block_catalogue;

    fn target() -> TopologyContractKey {
        crate::cosmos::topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone()
    }

    fn attempt() -> RevisionAttempt {
        RevisionAttempt::new(1).expect("one is a valid attempt number")
    }

    fn statement_only_facts(shell_available: bool, test_runner_available: bool) -> PlannerFacts {
        PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available,
            reproduce_runner_available: false,
            model_provider_available: false,
            is_question: false,
            is_survey: false,
            has_stated_edits: false,
            statement: "add() subtracts instead of adding: fix the implementation".to_owned(),
            shell_available,
        }
    }

    #[test]
    fn falls_back_to_the_generic_shape_without_a_shell() {
        let facts = statement_only_facts(false, false);
        let (expression, extra) = plan(&facts, attempt()).expect("plan builds");
        let (generic_expression, generic_extra) =
            planner::statement_only_plan_body(&facts, attempt(), NOTES).expect("plan builds");
        assert_eq!(format!("{expression:?}"), format!("{generic_expression:?}"));
        assert_eq!(extra.len(), generic_extra.len());
    }

    #[test]
    fn falls_back_to_the_generic_shape_with_a_test_runner() {
        let facts = statement_only_facts(true, true);
        let (expression, extra) = plan(&facts, attempt()).expect("plan builds");
        let (generic_expression, generic_extra) =
            planner::statement_only_plan_body(&facts, attempt(), NOTES).expect("plan builds");
        assert_eq!(format!("{expression:?}"), format!("{generic_expression:?}"));
        assert_eq!(extra.len(), generic_extra.len());
    }

    #[test]
    fn substitutes_the_sphinx_shape_with_a_shell_and_no_test_runner() {
        let facts = statement_only_facts(true, false);
        let (expression, _extra) = plan(&facts, attempt()).expect("plan builds");
        let rendered = format!("{expression:?}");
        assert!(rendered.contains("find-mirrored-tests-0"));
        assert!(rendered.contains("sphinx.has-verification"));
        assert!(rendered.contains("sphinx.run-check-and-mirrored-tests"));
        assert!(rendered.contains("conclude"));
    }

    #[test]
    fn the_sphinx_plan_materialises_under_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts =
            crate::cosmos::gate_blocks::coding_target_contracts().expect("coding contract catalog");
        let target = target();
        let facts = statement_only_facts(true, false);

        let built_plan =
            planner::scripted_plan_generic(&facts, attempt(), &catalogue, &target, |f, a| {
                plan(f, a)
            })
            .expect("the sphinx expert plan builds");
        let graph = materialize(
            &built_plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the sphinx expert plan materialises");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }
}
