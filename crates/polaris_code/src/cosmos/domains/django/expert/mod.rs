//! Django expert notes and a focused post-patch regression gate.

use polaris_cosmos::composition::{BlockInstanceId, ConfigId, GateExpr, StepPlan};
use polaris_cosmos::contracts::{CompositionBlockKey, RevisionAttempt};

use crate::cosmos::gate_blocks::{configurable, existing, repair};
use crate::cosmos::planner::{self, PlannerFacts, StatementPlanBody};

/// The same expert instructions the `generic-notes` and `expert` arms carry byte for byte.
pub const NOTES: &str = include_str!("notes.md");

pub mod blocks;

fn step(
    name: &str,
    key: CompositionBlockKey,
    config: &ConfigId,
) -> Result<GateExpr, planner::PlannerError> {
    Ok(GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(name)?,
        key,
        config.clone(),
    )))
}

/// Build the statement-only Django plan, retaining the generic plan's model budgets.
///
/// When the host has no shell, the domain runner cannot execute; the generic
/// statement-only shape still carries the same expert notes.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    if !facts.shell_available {
        return planner::statement_only_plan_body(facts, attempt, NOTES);
    }

    // The Django runner itself needs a shell, not a declared ProjectTestRunner.
    // Reuse the generic plan's self-check model configs (including run_python, 30 turns,
    // 200k tokens, and review_before_submit) even when both are available.
    let mut self_check_facts = facts.clone();
    self_check_facts.test_runner_available = false;
    let (_, extra) = planner::statement_only_plan_body(&self_check_facts, attempt, NOTES)?;
    let plain = ConfigId::new("no-config")?;
    let propose = ConfigId::new("propose-edit")?;
    let repair_config = ConfigId::new("propose-edit-repair")?;

    let repair = GateExpr::Seq(vec![
        step(
            "propose-1",
            repair::resume_model_propose_key(),
            &repair_config,
        )?,
        step(
            "extract-1",
            repair::extract_patch_from_proposal_key(),
            &plain,
        )?,
        step("apply-1", configurable::apply_patch_intent_key(), &plain)?,
        step("django-verify-1", blocks::verify_repair_key(), &plain)?,
        step("judge-1", repair::judge_test_outcome_key(), &plain)?,
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
            &plain,
        )?,
        step("apply-0", configurable::apply_patch_intent_key(), &plain)?,
        step("django-verify-0", blocks::verify_regression_key(), &plain)?,
        step("judge-0", repair::judge_test_outcome_key(), &plain)?,
        GateExpr::Choose {
            predicate: crate::cosmos::gate_blocks::predicates::check_needs_repair_key(),
            when_true: Box::new(repair),
            when_false: Box::new(step(
                "affirm-0",
                configurable::affirm_assessment_key(),
                &plain,
            )?),
        },
        step("conclude", existing::conclude_single_pass_key(), &plain)?,
    ]);
    Ok((expression, extra))
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::{CompositionLimits, materialize};

    use super::*;
    use crate::cosmos::domains::{Arm, Domain, DomainSelection, plan_for_selection};
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};

    #[test]
    fn expert_plan_materializes_with_conservative_limits() {
        let facts = PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available: false,
            reproduce_runner_available: false,
            model_provider_available: true,
            is_question: false,
            is_survey: false,
            has_stated_edits: false,
            statement: "Fix a Django behavior regression".to_owned(),
            shell_available: true,
        };
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contract");
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        let plan = plan_for_selection(
            DomainSelection {
                domain: Domain::Django,
                arm: Arm::Expert,
            },
            &facts,
            RevisionAttempt::new(1).expect("first attempt"),
            &catalogue,
            &target,
        )
        .expect("expert plan builds");
        let graph = materialize(
            &plan,
            &catalogue,
            &CompositionLimits::conservative(),
            &contracts,
        )
        .expect("expert plan materializes");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }
}
