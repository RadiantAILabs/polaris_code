//! `SymPy` expert notes and the scoped-verification statement-only plan.

use polaris_cosmos::composition::{
    BlockInstanceId, CaseKey, ConfigId, GateExpr, StepPlan, SwitchCase,
};
use polaris_cosmos::contracts::{CompositionBlockKey, RevisionAttempt};

use crate::cosmos::gate_blocks::{configurable, existing, repair};
use crate::cosmos::planner::{self, PlannerError, PlannerFacts, StatementPlanBody};

/// Expert notes shared byte-for-byte by the `generic-notes` and `expert` arms.
pub const NOTES: &str = include_str!("notes.md");

fn step(name: &str, key: CompositionBlockKey, config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(name)?,
        key,
        config.clone(),
    )))
}

/// Retains the generic plan's runner and shell-unavailable branches, but verifies nearby
/// `SymPy` tests after a patch when Python is available.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    let (generic, configurations) = planner::statement_only_plan_body(facts, attempt, NOTES)?;
    if facts.test_runner_available || !facts.shell_available {
        return Ok((generic, configurations));
    }

    let no_config = ConfigId::new("no-config")?;
    let propose = ConfigId::new("propose-edit")?;
    let repair_propose = ConfigId::new("propose-edit-repair")?;
    let repair_branch = GateExpr::Seq(vec![
        step(
            "propose-1",
            repair::resume_model_propose_key(),
            &repair_propose,
        )?,
        step(
            "extract-1",
            repair::extract_patch_from_proposal_key(),
            &no_config,
        )?,
        step(
            "apply-1",
            configurable::apply_patch_intent_key(),
            &no_config,
        )?,
        step(
            "verify-2",
            blocks::run_scoped_verification_key(),
            &no_config,
        )?,
    ]);
    let expression = GateExpr::Seq(vec![
        step(
            "propose-0",
            repair::model_propose_conversation_key(),
            &propose,
        )?,
        step(
            "extract-0",
            repair::extract_patch_from_proposal_key(),
            &no_config,
        )?,
        step(
            "apply-0",
            configurable::apply_patch_intent_key(),
            &no_config,
        )?,
        step(
            "verify-1",
            blocks::run_scoped_verification_key(),
            &no_config,
        )?,
        GateExpr::Switch {
            discriminator: blocks::verification_state_key(),
            cases: vec![
                SwitchCase::new(
                    CaseKey::new("passed")?,
                    step(
                        "affirm-1",
                        configurable::affirm_assessment_key(),
                        &no_config,
                    )?,
                ),
                SwitchCase::new(CaseKey::new("failed")?, repair_branch),
                SwitchCase::new(
                    CaseKey::new("unavailable")?,
                    step("judge-patch", repair::judge_patch_intent_key(), &no_config)?,
                ),
            ],
            default: None,
        },
        step("conclude", existing::conclude_single_pass_key(), &no_config)?,
    ]);
    Ok((expression, configurations))
}

/// Deterministic SymPy verification block and discriminator.
pub mod blocks;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    use polaris::graph::GraphExecutor;
    use polaris::system::param::SystemContext;
    use polaris::system::resource::Resources;
    use polaris_cosmos::composition::{CompositionLimits, materialize};

    use super::*;
    use crate::cosmos::gate_blocks::configurable::{
        ModelStepArtifact, ModelStepProvider, ScriptedModelStep,
    };
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};
    use crate::cosmos::payload::CodingWorkspace;
    use crate::cosmos::task::{CodingIntent, CodingTask, ExactEdit, SuccessCheck};
    use crate::usage::TokenMeter;

    fn facts(shell: bool, runner: bool) -> PlannerFacts {
        PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available: runner,
            reproduce_runner_available: false,
            model_provider_available: false,
            is_question: false,
            is_survey: false,
            has_stated_edits: false,
            statement: "fix a symbolic operation".to_owned(),
            shell_available: shell,
        }
    }

    #[test]
    fn scoped_plan_materializes_with_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("contract catalogue");
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        let attempt = RevisionAttempt::new(1).expect("first attempt");
        let plan = planner::scripted_plan_generic(
            &facts(true, false),
            attempt,
            &catalogue,
            &target,
            super::plan,
        )
        .expect("SymPy plan builds");
        materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("SymPy plan materializes");
    }

    #[test]
    fn runner_and_no_shell_retain_the_generic_shape() {
        let attempt = RevisionAttempt::new(1).expect("first attempt");
        for (shell, runner) in [(false, false), (true, true)] {
            let (expert, _) = plan(&facts(shell, runner), attempt).expect("expert body");
            let (generic, _) =
                planner::statement_only_plan_body(&facts(shell, runner), attempt, NOTES)
                    .expect("generic body");
            assert_eq!(expert, generic);
        }
    }

    #[tokio::test]
    async fn an_absolute_workspace_edit_concludes_the_statement_only_plan() {
        let dir = tempfile::tempdir().expect("temporary tree");
        std::fs::create_dir_all(dir.path().join("sympy/core")).expect("source directory");
        std::fs::write(dir.path().join("sympy/core/basic.py"), "value = 1\n").expect("source file");
        let task = CodingTask::from_statement(
            "sympy-scoped-plan-fixture",
            dir.path(),
            "correct the symbolic value",
            CodingIntent::Careful,
            SuccessCheck::stated_edits(),
        )
        .expect("statement task");
        let workspace = CodingWorkspace::for_task(&task);
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).expect("shim directory");
        let python = bin.join("python");
        std::fs::write(&python, "#!/bin/sh\nexec python3 \"$@\"\n").expect("python shim");
        let mut permissions = std::fs::metadata(&python)
            .expect("shim metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&python, permissions).expect("executable shim");
        let shell = polaris::shell::ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_path_buf()])
                .with_allowed_commands(vec!["python *".to_owned()])
                .with_path(format!("{}:/usr/bin:/bin", bin.display())),
        );
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("contract catalogue");
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        let plan = planner::scripted_plan_generic(
            &facts(true, false),
            RevisionAttempt::new(1).expect("first attempt"),
            &catalogue,
            &target,
            super::plan,
        )
        .expect("SymPy plan");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("materialized plan");
        let mut globals = Resources::new();
        globals.insert(shell);
        let mut context = SystemContext::with_globals(Arc::new(globals));
        context.insert(crate::cosmos::gate_blocks::configurable::ModelStepHandoff::default());
        context.insert(workspace.clone());
        context.insert(TokenMeter::default());
        context.insert(ModelStepProvider::new(ScriptedModelStep::new(
            ModelStepArtifact::PatchIntent {
                edits: vec![
                    ExactEdit::new(
                        dir.path().join("sympy/core/basic.py").to_string_lossy(),
                        "value = 1",
                        "value = 2",
                    )
                    .expect("exact edit"),
                ],
                check: Some("assert 2 == 2".to_owned()),
            },
        )));
        let execution = GraphExecutor::new()
            .execute(&graph, &mut context, None, None)
            .await
            .expect("SymPy plan executes");
        assert!(
            execution
                .output::<polaris_cosmos::contracts::AssessmentConclusion>()
                .is_some()
        );
        assert_eq!(
            workspace.read("sympy/core/basic.py").expect("read back"),
            "value = 2\n"
        );
    }
}
