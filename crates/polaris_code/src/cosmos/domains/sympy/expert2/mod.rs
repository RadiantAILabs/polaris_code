//! `SymPy` `expert2` arm: trace the owning layer, reproduce the symbolic
//! contract, then propose a patch before the established scoped verification
//! and repair.
//!
//! The three stages follow the general `SymPy` research workflow: find the
//! public API's actual owner (which may be a delegated hook), establish a
//! failing public example and a contrast, then change the smallest owner and
//! cover that behavior. The first two stages return typed, bounded findings;
//! the last returns the same patch intent the `expert` arm's downstream gates
//! already consume. No issue-class keyword
//! switch is used because a public wrapper can delegate to another package.

use polaris_cosmos::composition::{BlockInstanceId, ConfigId, GateExpr, StepPlan};
use polaris_cosmos::contracts::{CompositionBlockKey, RevisionAttempt};

use crate::cosmos::gate_blocks::configurable::{
    ContextMode, ModelStepConfig, ModelStepOutputKind, TokenBudget, ToolSubset,
};
use crate::cosmos::gate_blocks::fine;
use crate::cosmos::planner::{PlannerError, PlannerFacts, StatementPlanBody};

const WORKSPACE_RULE: &str = "Work only with the checkout already in the workspace: do not download, install or fetch another version of the project or its upstream changes. Installing a missing third-party dependency is fine.";
const LOCATE_BUDGET: u32 = 40_000;
const REPRODUCE_BUDGET: u32 = 50_000;
const PROPOSE_BUDGET: u32 = 110_000;
const LOCATE_TURNS: u16 = 6;
const REPRODUCE_TURNS: u16 = 7;
const PROPOSE_TURNS: u16 = 15;

fn step(name: &str, key: CompositionBlockKey, config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(name)?,
        key,
        config.clone(),
    )))
}

fn instruction(body: &str, statement: &str) -> String {
    // The notes are included literally, with no editing or paraphrase.
    format!(
        "{body}\n\nIssue:\n{statement}\n\n{WORKSPACE_RULE}\n\nDomain notes:\n{}",
        super::expert::NOTES
    )
}

/// The `SymPy` statement-only plan on the finer control surface.
///
/// The stage budgets total 200,000 tokens and 28 turns, below the `expert`
/// arm's 200,000 tokens and 30 turns for its initial proposal. Its one repair
/// keeps the `expert` arm's own 200,000-token, 30-turn configuration. The existing `expert` tail
/// retains the scoped `SymPy` test/doctest gate and its three-way verdict when
/// shell execution is available; other host shapes retain their original
/// fail-closed downstream gates.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    let (expression, mut configurations) = super::expert::plan(facts, attempt)?;
    let GateExpr::Seq(mut tail) = expression else {
        unreachable!("the SymPy expert plan is a sequence")
    };
    tail.remove(0); // replace only the `expert` arm's coarse initial proposal

    let read_tools = vec![
        "read_file".to_owned(),
        "list_dir".to_owned(),
        "search".to_owned(),
    ];
    let mut check_tools = read_tools.clone();
    let can_run_python = facts.shell_available && !facts.test_runner_available;
    if can_run_python {
        check_tools.push("run_python".to_owned());
    }

    let locate_id = ConfigId::new("sympy-c2-locate")?;
    let reproduce_id = ConfigId::new("sympy-c2-reproduce")?;
    let propose_id = ConfigId::new("propose-edit")?;
    let locate = ModelStepConfig::new(
        "sympy-locate-owner".to_owned(),
        instruction(
            "Trace the reported public operation to the code that actually owns its behavior. Read the public entry point, follow dispatch or delegation into its hook, converter, representation or consumer, and inspect nearby tests. Search relevant sibling hooks and downstream callers when a shared contract is involved. In at most 8000 characters, name the code path, evidence, likely owner, nearby tests and remaining uncertainty. Do not propose an edit yet.",
            &facts.statement,
        ),
        ToolSubset::Only(read_tools),
        TokenBudget(LOCATE_BUDGET),
        ModelStepOutputKind::Answer,
    )
    .with_max_turns(LOCATE_TURNS)
    .with_context_mode(ContextMode::Brief)
    .encode()?;
    let reproduce = ModelStepConfig::new(
        "sympy-reproduce-contract".to_owned(),
        instruction(
            "Establish the public behavior before editing. Make a short script with explicit imports, the reported call and an assertion for the expected contract; run it with run_python when available. Check one nearby contrast and record observed versus expected results. Use exact small values or an independent identity for mathematics, args or srepr for structure, and exact strings only for a printer contract. Distinguish a missing optional dependency or unusable check from a genuine failing assertion. In at most 8000 characters, summarize the reproducer, contrast, output, suspected boundary and uncertainties. Do not propose the patch yet.",
            &facts.statement,
        ),
        ToolSubset::Only(check_tools.clone()),
        TokenBudget(REPRODUCE_BUDGET),
        ModelStepOutputKind::Answer,
    )
    .with_max_turns(REPRODUCE_TURNS)
    .with_context_mode(ContextMode::Continue)
    .encode()?;
    let propose = ModelStepConfig::new(
        "sympy-propose-fix".to_owned(),
        instruction(
            "Propose the smallest exact-string patch at the owning layer, with a nearby public-behavior regression and a contrasting boundary case. Use the location and reproduction findings below; inspect more code if they leave uncertainty. Preserve Basic reconstruction, three-valued assumptions, expression form, supported arguments and downstream callers where relevant. When run_python is available, run the focused check before submitting and include a check script when possible. Review the proposed edits against the reported behavior, sibling variants and compatibility before submitting; state anything unverified.",
            &facts.statement,
        ),
        ToolSubset::Only(check_tools),
        TokenBudget(PROPOSE_BUDGET),
        ModelStepOutputKind::PatchIntent,
    )
    .with_max_turns(PROPOSE_TURNS)
    .with_context_mode(ContextMode::Continue)
    .with_review_before_submit()
    .encode()?;

    configurations.retain(|(id, _)| id != &propose_id);
    // The `expert` arm's runner branch has no Python tool and consequently omitted the
    // workspace rule from its repair prompt. `expert2` applies it to every model step.
    let repair_id = ConfigId::new("propose-edit-repair")?;
    if let Some((_, repair)) = configurations.iter_mut().find(|(id, _)| id == &repair_id) {
        let mut decoded = ModelStepConfig::decode(repair)
            .map_err(|error| PlannerError::InvalidConfiguration(error.to_string()))?;
        if !decoded.instruction.contains(WORKSPACE_RULE) {
            decoded
                .instruction
                .push_str(&format!("\n\n{WORKSPACE_RULE}"));
            *repair = decoded.encode()?;
        }
    }
    configurations.extend([
        (locate_id.clone(), locate),
        (reproduce_id.clone(), reproduce),
        (propose_id.clone(), propose),
    ]);

    let mut stages = vec![
        step("sympy-c2-locate", fine::locate_key(), &locate_id)?,
        step("sympy-c2-reproduce", fine::reproduce_key(), &reproduce_id)?,
        step(
            "propose-0",
            if can_run_python {
                fine::propose_conversation_key()
            } else {
                fine::propose_key()
            },
            &propose_id,
        )?,
    ];
    stages.append(&mut tail);
    Ok((GateExpr::Seq(stages), configurations))
}

/// The `expert2` arm registers no new blocks: the typed fine stages and the
/// `expert` arm's scoped verifier already provide the required boundaries and
/// result states.
pub mod blocks {
    use polaris_cosmos::composition::{CompositionCatalogue, CompositionCatalogueError};
    use polaris_cosmos::contracts::CompositionBlockKey;

    /// `expert2` adds no block keys to the catalogue.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        Vec::new()
    }

    /// `expert2` needs no additional registration.
    ///
    /// # Errors
    /// Reserved for catalogue registration errors if an `expert2` block is added.
    pub fn register_all(
        _catalogue: &mut CompositionCatalogue,
    ) -> Result<(), CompositionCatalogueError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::{CompositionLimits, materialize};

    use super::*;
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};
    use crate::cosmos::planner;

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
    fn stages_and_budgets_are_pinned_for_every_host_shape() {
        let attempt = RevisionAttempt::new(1).expect("attempt");
        for (shell, runner) in [(false, false), (true, false), (true, true)] {
            let facts = facts(shell, runner);
            let (expert2, configs) = plan(&facts, attempt).expect("expert2 body");
            let (c, c_configs) = super::super::expert::plan(&facts, attempt).expect("expert body");
            let GateExpr::Seq(expert2_steps) = expert2 else {
                panic!("expert2 sequence")
            };
            let GateExpr::Seq(c_steps) = c else {
                panic!("expert sequence")
            };
            assert_eq!(expert2_steps.len(), c_steps.len() + 2);
            assert_eq!(&expert2_steps[3..], &c_steps[1..]);
            for (index, key) in [
                fine::locate_key(),
                fine::reproduce_key(),
                if shell && !runner {
                    fine::propose_conversation_key()
                } else {
                    fine::propose_key()
                },
            ]
            .iter()
            .enumerate()
            {
                let GateExpr::Step(stage) = &expert2_steps[index] else {
                    panic!("stage step")
                };
                assert_eq!(stage.block(), key);
            }
            let get = |id: &str| {
                let id = ConfigId::new(id).expect("config ID");
                ModelStepConfig::decode(
                    &configs
                        .iter()
                        .find(|(name, _)| name == &id)
                        .expect("stage config")
                        .1,
                )
                .expect("valid model config")
            };
            let stages = [
                get("sympy-c2-locate"),
                get("sympy-c2-reproduce"),
                get("propose-edit"),
            ];
            assert_eq!(
                stages
                    .iter()
                    .map(|stage| stage.max_turns)
                    .collect::<Vec<_>>(),
                [LOCATE_TURNS, REPRODUCE_TURNS, PROPOSE_TURNS]
            );
            assert_eq!(
                stages
                    .iter()
                    .map(|stage| stage.budget.0)
                    .collect::<Vec<_>>(),
                [LOCATE_BUDGET, REPRODUCE_BUDGET, PROPOSE_BUDGET]
            );
            assert_eq!(
                stages
                    .iter()
                    .map(|stage| stage.context_mode)
                    .collect::<Vec<_>>(),
                [
                    ContextMode::Brief,
                    ContextMode::Continue,
                    ContextMode::Continue
                ]
            );
            assert_eq!(
                stages.iter().map(|stage| stage.output).collect::<Vec<_>>(),
                [
                    ModelStepOutputKind::Answer,
                    ModelStepOutputKind::Answer,
                    ModelStepOutputKind::PatchIntent
                ]
            );
            assert!(stages.iter().all(|stage| {
                stage.instruction.contains(super::super::expert::NOTES)
                    && stage.instruction.contains(WORKSPACE_RULE)
            }));
            assert!(!stages[0].review_before_submit);
            assert!(!stages[1].review_before_submit);
            assert!(stages[2].review_before_submit);
            assert_eq!(
                stages
                    .iter()
                    .map(|stage| u32::from(stage.max_turns))
                    .sum::<u32>(),
                28
            );
            assert_eq!(
                stages.iter().map(|stage| stage.budget.0).sum::<u32>(),
                200_000
            );
            let c_initial = ModelStepConfig::decode(
                &c_configs
                    .iter()
                    .find(|(name, _)| name == &ConfigId::new("propose-edit").expect("ID"))
                    .expect("C initial")
                    .1,
            )
            .expect("C config");
            assert_eq!(c_initial.max_turns, 30);
            assert_eq!(c_initial.budget.0, 200_000);
            if shell || runner {
                let repair = get("propose-edit-repair");
                assert_eq!(repair.max_turns, 30);
                assert_eq!(repair.budget.0, 200_000);
                assert!(repair.instruction.contains(super::super::expert::NOTES));
                assert!(repair.instruction.contains(WORKSPACE_RULE));
            }
        }
    }

    #[test]
    fn expert2_materializes_and_validates_with_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("contracts");
        let target = crate::cosmos::topology::contract()
            .expect("contract")
            .key()
            .clone();
        let attempt = RevisionAttempt::new(1).expect("attempt");
        for (shell, runner) in [(false, false), (true, false), (true, true)] {
            let plan = planner::scripted_plan_generic(
                &facts(shell, runner),
                attempt,
                &catalogue,
                &target,
                super::plan,
            )
            .expect("expert2 plan");
            let graph = materialize(
                &plan,
                &catalogue,
                &CompositionLimits::conservative(),
                &contracts,
            )
            .expect("expert2 materialization");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        }
    }
}
