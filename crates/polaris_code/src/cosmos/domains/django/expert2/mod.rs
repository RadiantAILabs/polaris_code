//! Django `expert2` arm: bounded owner tracing, reproduction, proposal, and the
//! `expert` arm's existing regression and repair gates.

use polaris_cosmos::composition::{BlockInstanceId, ConfigId, GateExpr, StepPlan};
use polaris_cosmos::contracts::{CompositionBlockKey, RevisionAttempt};

use crate::cosmos::gate_blocks::configurable::{
    ContextMode, ModelStepConfig, ModelStepOutputKind, TokenBudget, ToolSubset,
};
use crate::cosmos::gate_blocks::fine;
use crate::cosmos::planner::{PlannerError, PlannerFacts, StatementPlanBody};

const WORKSPACE_RULE: &str = "Work only with the checkout already in the workspace: do not download, install or fetch another version of the project or its upstream changes. Installing a missing third-party dependency is fine.";

// The `expert` arm's initial proposal has 30 turns and 200,000 tokens. Its repair has
// another 30 turns and 200,000 tokens and remains unchanged below.
const STAGE_TURNS: [u16; 3] = [6, 8, 16];
const STAGE_TOKENS: [u32; 3] = [40_000, 50_000, 110_000];

fn step(name: &str, key: CompositionBlockKey, config: &ConfigId) -> Result<GateExpr, PlannerError> {
    Ok(GateExpr::Step(StepPlan::new(
        BlockInstanceId::new(name)?,
        key,
        config.clone(),
    )))
}

struct StageSpec {
    role: &'static str,
    instruction: String,
    tools: Vec<String>,
    budget: u32,
    turns: u16,
    output: ModelStepOutputKind,
    context: ContextMode,
    review: bool,
}

fn stage_config(
    spec: StageSpec,
) -> Result<polaris_cosmos::contracts::ResolvedConfigurationRef, PlannerError> {
    // The expert notes are appended verbatim to every staged instruction.
    let instruction = format!(
        "{}\n\n{WORKSPACE_RULE}\n\nDomain notes:\n{}",
        spec.instruction,
        super::expert::NOTES
    );
    let mut config = ModelStepConfig::new(
        spec.role.to_owned(),
        instruction,
        ToolSubset::Only(spec.tools),
        TokenBudget(spec.budget),
        spec.output,
    )
    .with_max_turns(spec.turns)
    .with_context_mode(spec.context);
    if spec.review {
        config = config.with_review_before_submit();
    }
    Ok(config.encode()?)
}

/// Build the `expert2` statement-only edit plan from the Django research workflow.
///
/// The `expert` arm's downstream apply, regression verifier, behavior-only repair, and
/// conclusion remain. The first proposal is split where a maintainer changes
/// questions: owner, failing behavior, then fix.
pub fn plan(facts: &PlannerFacts, attempt: RevisionAttempt) -> StatementPlanBody {
    let (expression, mut extra) = super::expert::plan(facts, attempt)?;
    let GateExpr::Seq(mut downstream) = expression else {
        unreachable!("the expert statement-only plan is a sequence")
    };
    downstream.remove(0);
    let proposal_id = ConfigId::new("propose-edit")?;
    extra.retain(|(id, _)| id != &proposal_id);

    let locate_id = ConfigId::new("django-c2-locate")?;
    let reproduce_id = ConfigId::new("django-c2-reproduce")?;
    let read_tools = ["read_file", "list_dir", "search"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut check_tools = read_tools.clone();
    if facts.shell_available {
        check_tools.push("run_python".to_owned());
    }

    // The Django research workflow maps public APIs through producer, dispatch,
    // and consumer. A stage boundary keeps this search from becoming a fix.
    let locate = stage_config(StageSpec {
        role: "django-locate",
        instruction: format!(
            "Locate the owner of this behavior in the checked-out Django tree. First list the public operation, actual and expected result, and every named boundary. Search the public API and nearest core tests, then follow the value through its producer, dispatch hook, and consumer. Distinguish ORM QuerySet/Query/compiler/converters, migrations state/optimizer/writer, forms clean/metaclass, or middleware/session chains only where relevant. Confirm names in this checkout; narrow truncated searches. Submit an answer of at most 8000 characters with specific files, symbols, observed flow, and unresolved alternatives. Do not propose an edit yet.\n\nIssue:\n{}",
            facts.statement
        ),
        tools: read_tools.clone(),
        budget: STAGE_TOKENS[0],
        turns: STAGE_TURNS[0],
        output: ModelStepOutputKind::Answer,
        context: ContextMode::Brief,
        review: false,
    })?;
    // The Django research workflow distinguishes core tests from small
    // settings-configured reproductions; the owner determines which to use.
    let reproduce = stage_config(StageSpec {
        role: "django-reproduce",
        instruction: format!(
            "Use the locate findings to establish the smallest failing behavior before selecting a fix. Build a matrix of named modes and boundaries. Choose a nearby existing tests/<component>/ method or a short settings-configured Python assertion; use run_python when available, with Django's tests/runtests.py and a dotted label for core tests. Record the exact check and its observed result on this checkout. If setup, dependencies, backend access, or the 30-second tool limit prevent a run, say exactly what remains unverified and select the post-patch check. Submit an answer of at most 8000 characters with evidence, chosen test neighborhood, expected assertion, and limits. Do not propose the final patch yet.\n\nIssue:\n{}",
            facts.statement
        ),
        tools: check_tools.clone(),
        budget: STAGE_TOKENS[1],
        turns: STAGE_TURNS[1],
        output: ModelStepOutputKind::Answer,
        context: ContextMode::Continue,
        review: false,
    })?;
    // The Django research workflow calls for the smallest owner-level change,
    // sibling-path audit, and boundary tests. Keep the `expert` arm's
    // pre-submit review.
    let propose = stage_config(StageSpec {
        role: "django-propose",
        instruction: format!(
            "Propose the smallest exact-string patch at the layer that owns the reproduced invariant. Use the locate and reproduction findings below; inspect further as needed. Add a fail-before/pass-after regression method to an existing tests/<component>/ file when possible, and submit a check script when a short independent assertion is feasible. Audit the named paired operations, subclass hooks, backend overrides, migration import paths, stored formats, and relevant docs or deprecation obligations. Run a focused check with run_python when available. State which modes or backends remain unverified. Before final submission, compare the proposed edits and tests against every boundary in the issue.\n\nIssue:\n{}",
            facts.statement
        ),
        tools: check_tools,
        budget: STAGE_TOKENS[2],
        turns: STAGE_TURNS[2],
        output: ModelStepOutputKind::PatchIntent,
        context: ContextMode::Continue,
        review: true,
    })?;
    extra.extend([
        (locate_id.clone(), locate),
        (reproduce_id.clone(), reproduce),
        (proposal_id.clone(), propose),
    ]);

    let mut stages = vec![
        step("django-c2-locate", fine::locate_key(), &locate_id)?,
        step("django-c2-reproduce", fine::reproduce_key(), &reproduce_id)?,
        step(
            "propose-0",
            if facts.shell_available {
                fine::propose_conversation_key()
            } else {
                fine::propose_key()
            },
            &proposal_id,
        )?,
    ];
    stages.append(&mut downstream);
    Ok((GateExpr::Seq(stages), extra))
}

/// `expert2` adds no blocks: shared fine stages implement the typed handoffs, and
/// the `expert` arm's Django verifier implements the regression outcome states.
pub mod blocks {
    use polaris_cosmos::composition::{CompositionCatalogue, CompositionCatalogueError};
    use polaris_cosmos::contracts::CompositionBlockKey;

    /// `expert2`'s own block keys; all stages reuse registered blocks.
    #[must_use]
    pub fn keys() -> Vec<CompositionBlockKey> {
        Vec::new()
    }

    /// Register `expert2`-only blocks; the current plan needs none.
    ///
    /// # Errors
    /// Returns a catalogue error if future `expert2` blocks cannot be registered.
    pub fn register_all(
        _catalogue: &mut CompositionCatalogue,
    ) -> Result<(), CompositionCatalogueError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::{CompositionLimits, ConfigId, GateExpr, materialize};
    use polaris_cosmos::contracts::RevisionAttempt;

    use super::*;
    use crate::cosmos::domains::{Arm, Domain, DomainSelection, plan_for_selection};
    use crate::cosmos::gate_blocks::{coding_block_catalogue, coding_target_contracts};

    fn facts(shell: bool) -> PlannerFacts {
        PlannerFacts {
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
            shell_available: shell,
        }
    }

    fn config(plan: &polaris_cosmos::composition::GatePlan, id: &str) -> ModelStepConfig {
        let id = ConfigId::new(id).expect("configuration id");
        ModelStepConfig::decode(plan.configurations().get(&id).expect("configuration"))
            .expect("valid model step")
    }

    #[test]
    fn expert2_stage_boundaries_notes_and_budget_match_expert() {
        let catalogue = coding_block_catalogue();
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        for shell in [true, false] {
            let facts = facts(shell);
            let c = plan_for_selection(
                DomainSelection {
                    domain: Domain::Django,
                    arm: Arm::Expert,
                },
                &facts,
                RevisionAttempt::new(1).expect("attempt"),
                &catalogue,
                &target,
            )
            .expect("expert plan");
            let expert2 = plan_for_selection(
                DomainSelection {
                    domain: Domain::Django,
                    arm: Arm::Expert2,
                },
                &facts,
                RevisionAttempt::new(1).expect("attempt"),
                &catalogue,
                &target,
            )
            .expect("expert2 plan");
            let GateExpr::Seq(c_steps) = c.expression() else {
                panic!("expert sequence")
            };
            let GateExpr::Seq(expert2_steps) = expert2.expression() else {
                panic!("expert2 sequence")
            };
            let expected = [
                fine::locate_key(),
                fine::reproduce_key(),
                if shell {
                    fine::propose_conversation_key()
                } else {
                    fine::propose_key()
                },
            ];
            for (actual, key) in expert2_steps[..3].iter().zip(expected) {
                let GateExpr::Step(step) = actual else {
                    panic!("model stage")
                };
                assert_eq!(step.block(), &key);
            }
            assert_eq!(&expert2_steps[3..], &c_steps[1..]);

            let stages = ["django-c2-locate", "django-c2-reproduce", "propose-edit"]
                .map(|id| config(&expert2, id));
            assert_eq!(stages.each_ref().map(|step| step.max_turns), STAGE_TURNS);
            assert_eq!(stages.each_ref().map(|step| step.budget.0), STAGE_TOKENS);
            assert_eq!(
                stages.each_ref().map(|step| step.context_mode),
                [
                    ContextMode::Brief,
                    ContextMode::Continue,
                    ContextMode::Continue
                ]
            );
            assert_eq!(
                stages.each_ref().map(|step| step.output),
                [
                    ModelStepOutputKind::Answer,
                    ModelStepOutputKind::Answer,
                    ModelStepOutputKind::PatchIntent
                ]
            );
            assert!(stages[2].review_before_submit);
            assert!(stages[..2].iter().all(|step| !step.review_before_submit));
            for stage in &stages {
                assert!(stage.instruction.contains(WORKSPACE_RULE));
                assert!(stage.instruction.ends_with(super::super::expert::NOTES));
                assert!(
                    matches!(&stage.tool_subset, ToolSubset::Only(tools) if !tools.iter().any(|tool| tool == "edit_file"))
                );
            }
            assert!(stages[0].instruction.contains("dispatch hook"));
            assert!(stages[1].instruction.contains("smallest failing behavior"));
            assert!(
                stages[2].instruction.contains("sibling")
                    || stages[2].instruction.contains("paired operations")
            );
            let c_proposal = config(&c, "propose-edit");
            assert_eq!(
                STAGE_TURNS.iter().map(|v| u32::from(*v)).sum::<u32>(),
                u32::from(c_proposal.max_turns)
            );
            assert_eq!(STAGE_TOKENS.iter().sum::<u32>(), c_proposal.budget.0);
            let repair_id = ConfigId::new("propose-edit-repair").expect("repair id");
            assert_eq!(
                expert2.configurations().get(&repair_id),
                c.configurations().get(&repair_id)
            );
            if let Some(repair) = expert2.configurations().get(&repair_id) {
                let repair = ModelStepConfig::decode(repair).expect("repair config");
                assert!(repair.instruction.contains(WORKSPACE_RULE));
                assert!(repair.instruction.ends_with(super::super::expert::NOTES));
                assert_eq!(repair.max_turns, 30);
                assert_eq!(repair.budget.0, 200_000);
            }
        }
    }

    #[test]
    fn expert2_materializes_and_validates_under_conservative_limits() {
        let catalogue = coding_block_catalogue();
        let contracts = coding_target_contracts().expect("coding contracts");
        let target = crate::cosmos::topology::contract()
            .expect("coding contract")
            .key()
            .clone();
        for shell in [true, false] {
            let plan = plan_for_selection(
                DomainSelection {
                    domain: Domain::Django,
                    arm: Arm::Expert2,
                },
                &facts(shell),
                RevisionAttempt::new(1).expect("attempt"),
                &catalogue,
                &target,
            )
            .expect("expert2 plan");
            let graph = materialize(
                &plan,
                &catalogue,
                &CompositionLimits::conservative(),
                &contracts,
            )
            .expect("expert2 materializes");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        }
    }
}
