//! Domain plans: per-repository notes, extra gate blocks and a statement-only
//! plan, selected orthogonally to
//! [`super::planner::GatePlannerMode`]/[`super::composition::CompositionPolicy`]
//! so a run can compare a generic plan against domain-specialised ones.
//!
//! Each domain (`django`, `sympy`, `sphinx`) has two variants: `expert` (the
//! [`Arm::Expert`] notes and plan; [`Arm::GenericNotes`] also borrows its notes)
//! and `experience` (the [`Arm::Experience`] notes and plan, mined from past
//! transcripts rather than written from research). A variant that has nothing
//! domain-specific to add is identical to the generic plan: empty notes, no
//! extra gate blocks, and a statement-only plan that is
//! [`super::planner::statement_only_plan_body`] with its own (empty) notes. A
//! change to one variant touches only files inside its `<domain>/<variant>`
//! module — see `cosmos/domains/README.md` for what those files are and how to
//! test a change to one.
//!
//! [`plan_for_selection`] is the one seam a `gates`-policy turn reaches this
//! module through: [`super::planner::attempt_gate_plan`] calls it in place of
//! a plain [`super::planner::scripted_plan`] call whenever its planner mode
//! is [`super::planner::GatePlannerMode::Scripted`]. [`Arm::Generic`] —
//! this module's `Default`, installed whenever nothing configures
//! [`DomainSelection`] — dispatches straight to
//! [`super::planner::scripted_plan`], unmodified, regardless of
//! [`DomainSelection::domain`]: that single-line dispatch is what keeps the
//! generic arm's plan unchanged by domain plans.

mod django;
pub(super) mod sphinx;
pub(super) mod sympy;

use std::fmt;

use polaris_cosmos::composition::{
    CompositionCatalogue, CompositionCatalogueError, CompositionCatalogueSnapshot, GatePlan,
};
use polaris_cosmos::contracts::{CompositionBlockKey, RevisionAttempt};
use polaris_cosmos::topology::TopologyContractKey;

use super::planner::{self, PlannerError, PlannerFacts, StatementPlanBody};

/// Which repository's domain plan and notes a run selects. [`Domain::None`]
/// pairs with the domain-independent generic and loop arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Domain {
    /// No domain selected.
    #[default]
    None,
    /// Django.
    Django,
    /// `SymPy`.
    Sympy,
    /// Sphinx.
    Sphinx,
}

impl Domain {
    /// The `POLARIS_CODE_DOMAIN`/eval-harness `--domain` spelling of this
    /// domain.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Django => "django",
            Self::Sympy => "sympy",
            Self::Sphinx => "sphinx",
        }
    }

    /// Parses the explicit spelling above.
    ///
    /// # Errors
    /// Returns [`DomainSelectionError::UnknownDomain`] for anything else.
    pub fn parse(value: &str) -> Result<Self, DomainSelectionError> {
        match value {
            "none" => Ok(Self::None),
            "django" => Ok(Self::Django),
            "sympy" => Ok(Self::Sympy),
            "sphinx" => Ok(Self::Sphinx),
            _ => Err(DomainSelectionError::UnknownDomain(value.to_owned())),
        }
    }
}

impl fmt::Display for Domain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which plan and notes treatment a run selects: the fixed-plan
/// arms, [`Arm::Loop`], [`Arm::Fine`], [`Arm::FineContext`], and [`Arm::Composed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Arm {
    /// The unmodified generic gate plan; no domain knowledge. Pairs with any
    /// [`Domain`], including [`Domain::None`], and ignores it either way.
    #[default]
    Generic,
    /// [`Arm::Generic`]'s own plan shape, with the selected domain's expert
    /// notes added to every statement-only model step's instruction.
    GenericNotes,
    /// The selected domain's own researched plan and notes.
    Expert,
    /// The selected domain's own plan and notes mined from transcripts.
    Experience,
    /// "One generic loop": one model conversation, with a real edit tool, and
    /// nothing composed around it beyond the one step every gate plan needs to
    /// close the coding contract. Domain-independent — it reads no notes and no
    /// domain plan — so unlike the other three non-generic arms, it pairs
    /// only with [`Domain::None`]; pairing it with a real domain would carry
    /// a domain selection that changes nothing, which is worth refusing
    /// rather than silently allowing.
    Loop,
    /// One conversation with a general shell and a 100-turn ceiling.
    Lean,
    /// Runtime model composition with this domain's expert notes, worked
    /// plan, and expert blocks that the flat gate grammar can wire.
    Composed,
    /// Generic staged proposal: locate, reproduce, propose, then the generic
    /// arm's gates.
    Fine,
    /// The same staged proposal, with conversations carried between stages.
    FineContext,
    /// The selected domain's expert plan rebuilt on the finer surface
    /// (staged blocks, context handoff, the 64-block envelope).
    Expert2,
}

impl Arm {
    /// The `POLARIS_CODE_ARM`/eval-harness `--arm` spelling of this arm.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::GenericNotes => "generic-notes",
            Self::Expert => "expert",
            Self::Experience => "experience",
            Self::Loop => "loop",
            Self::Lean => "lean",
            Self::Composed => "composed",
            Self::Fine => "fine",
            Self::FineContext => "fine-ctx",
            Self::Expert2 => "expert2",
        }
    }

    /// Parses the explicit spelling above.
    ///
    /// # Errors
    /// Returns [`DomainSelectionError::UnknownArm`] for anything else.
    pub fn parse(value: &str) -> Result<Self, DomainSelectionError> {
        match value {
            "generic" => Ok(Self::Generic),
            "generic-notes" => Ok(Self::GenericNotes),
            "expert" => Ok(Self::Expert),
            "experience" => Ok(Self::Experience),
            "loop" => Ok(Self::Loop),
            "lean" => Ok(Self::Lean),
            "composed" => Ok(Self::Composed),
            "fine" => Ok(Self::Fine),
            "fine-ctx" => Ok(Self::FineContext),
            "expert2" => Ok(Self::Expert2),
            _ => Err(DomainSelectionError::UnknownArm(value.to_owned())),
        }
    }
}

impl fmt::Display for Arm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why an explicit domain/arm configuration was refused.
#[derive(Debug)]
pub enum DomainSelectionError {
    /// [`Domain::parse`] saw a value that names none of the registered
    /// domains.
    UnknownDomain(String),
    /// [`Arm::parse`] saw a value that names none of the registered arms.
    UnknownArm(String),
    /// A domain-dependent arm was requested alongside [`Domain::None`].
    ArmNeedsDomain(Arm),
    /// [`Arm::Loop`] was requested alongside a real [`Domain`]: it reads no
    /// notes and no domain plan, so pairing it with one would carry a
    /// selection that changes nothing.
    LoopNeedsNoDomain,
    /// Lean is independent of repository-specific domain plans.
    LeanNeedsNoDomain,
    /// [`Arm::Fine`] was requested alongside a real [`Domain`].
    FineNeedsNoDomain,
    /// [`Arm::FineContext`] was requested alongside a real [`Domain`].
    FineContextNeedsNoDomain,
}

impl fmt::Display for DomainSelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDomain(value) => write!(
                f,
                "unknown domain '{value}'; expected none, django, sympy, or sphinx"
            ),
            Self::UnknownArm(value) => write!(
                f,
                "unknown arm '{value}'; expected generic, generic-notes, expert, experience, loop, lean, composed, fine, fine-ctx, or expert2"
            ),
            Self::ArmNeedsDomain(arm) => {
                write!(f, "arm '{}' needs a domain other than none", arm.as_str())
            }
            Self::LeanNeedsNoDomain => {
                f.write_str("arm 'lean' reads no domain plan; it runs only with domain none")
            }
            Self::LoopNeedsNoDomain => {
                f.write_str("arm 'loop' reads no domain plan; it runs only with domain none")
            }
            Self::FineNeedsNoDomain => {
                f.write_str("arm 'fine' reads no domain plan; it runs only with domain none")
            }
            Self::FineContextNeedsNoDomain => {
                f.write_str("arm 'fine-ctx' reads no domain plan; it runs only with domain none")
            }
        }
    }
}

impl std::error::Error for DomainSelectionError {}

/// The resolved (domain, arm) pair a `gates`-policy turn composes the
/// statement-only edit shape under. Installed as a session global exactly
/// like [`super::planner::GatePlannerMode`]; absent, `Default` (no domain,
/// the generic arm) runs the unmodified generic plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DomainSelection {
    /// The selected repository, or [`Domain::None`].
    pub domain: Domain,
    /// The selected plan/notes treatment.
    pub arm: Arm,
}

impl polaris::system::resource::GlobalResource for DomainSelection {}

impl DomainSelection {
    /// Resolves and validates one (domain, arm) pair from their explicit
    /// spellings.
    ///
    /// # Errors
    /// Returns [`DomainSelectionError`] for an unknown domain, an unknown
    /// arm, or an arm that needs a domain paired with [`Domain::None`].
    pub fn resolve(domain: &str, arm: &str) -> Result<Self, DomainSelectionError> {
        let domain = Domain::parse(domain)?;
        let arm = Arm::parse(arm)?;
        if matches!(domain, Domain::None)
            && !matches!(
                arm,
                Arm::Generic | Arm::Loop | Arm::Lean | Arm::Fine | Arm::FineContext
            )
        {
            return Err(DomainSelectionError::ArmNeedsDomain(arm));
        }
        if matches!(arm, Arm::Lean) && !matches!(domain, Domain::None) {
            return Err(DomainSelectionError::LeanNeedsNoDomain);
        }
        if matches!(arm, Arm::Loop) && !matches!(domain, Domain::None) {
            return Err(DomainSelectionError::LoopNeedsNoDomain);
        }
        if matches!(arm, Arm::Fine) && !matches!(domain, Domain::None) {
            return Err(DomainSelectionError::FineNeedsNoDomain);
        }
        if matches!(arm, Arm::FineContext) && !matches!(domain, Domain::None) {
            return Err(DomainSelectionError::FineContextNeedsNoDomain);
        }
        Ok(Self { domain, arm })
    }
}

/// `domain`'s expert-variant notes, empty for [`Domain::None`].
pub(crate) fn expert_notes(domain: Domain) -> &'static str {
    match domain {
        Domain::None => "",
        Domain::Django => django::expert::NOTES,
        Domain::Sympy => sympy::expert::NOTES,
        Domain::Sphinx => sphinx::expert::NOTES,
    }
}

/// `domain`'s expert-variant statement-only plan.
fn expert_plan(
    domain: Domain,
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    match domain {
        Domain::None => planner::statement_only_plan_body(facts, attempt, ""),
        Domain::Django => django::expert::plan(facts, attempt),
        Domain::Sympy => sympy::expert::plan(facts, attempt),
        Domain::Sphinx => sphinx::expert::plan(facts, attempt),
    }
}

/// The `expert2` arm: `domain`'s expert-variant statement-only plan.
fn expert2_plan(
    domain: Domain,
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    match domain {
        Domain::None => planner::statement_only_plan_body(facts, attempt, ""),
        Domain::Django => django::expert2::plan(facts, attempt),
        Domain::Sympy => sympy::expert2::plan(facts, attempt),
        Domain::Sphinx => sphinx::expert2::plan(facts, attempt),
    }
}

/// `domain`'s experience-variant statement-only plan.
fn experience_plan(
    domain: Domain,
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
) -> StatementPlanBody {
    match domain {
        Domain::None => planner::statement_only_plan_body(facts, attempt, ""),
        Domain::Django => django::experience::plan(facts, attempt),
        Domain::Sympy => sympy::experience::plan(facts, attempt),
        Domain::Sphinx => sphinx::experience::plan(facts, attempt),
    }
}

/// Builds one turn's gate plan for `selection`.
///
/// [`Arm::Generic`] calls [`planner::scripted_plan`] completely unchanged,
/// regardless of `selection.domain` — this is what keeps the generic arm's
/// plan unchanged by domain plans. Every other arm calls
/// [`planner::scripted_plan_generic`] with the same non-statement-only
/// branches, substituting only the statement-only plan body: [`Arm::GenericNotes`]
/// is [`planner::statement_only_plan_body`] with the domain's expert notes;
/// [`Arm::Expert`]/[`Arm::Experience`] are the domain variant's own `plan`;
/// [`Arm::Loop`] is [`planner::loop_statement_only_plan_body`], and
/// [`Arm::Fine`] and [`Arm::FineContext`] share the fine plan with different
/// handoff modes, ignoring `selection.domain` (always [`Domain::None`] — see
/// [`DomainSelection::resolve`]). [`Arm::Composed`] is only used by the model
/// planner, so this scripted dispatch returns the generic plan for it; the
/// model path receives the selected domain through
/// [`planner::attempt_gate_plan`].
///
/// # Errors
/// Returns whatever [`PlannerError`] the underlying plan construction does.
pub fn plan_for_selection(
    selection: DomainSelection,
    facts: &PlannerFacts,
    attempt: RevisionAttempt,
    catalogue: &CompositionCatalogueSnapshot,
    target: &TopologyContractKey,
) -> Result<GatePlan, PlannerError> {
    match selection.arm {
        Arm::Generic => planner::scripted_plan(facts, attempt, catalogue, target),
        Arm::GenericNotes => {
            let notes = expert_notes(selection.domain);
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                planner::statement_only_plan_body(f, a, notes)
            })
        }
        Arm::Expert => planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
            expert_plan(selection.domain, f, a)
        }),
        Arm::Experience => {
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                experience_plan(selection.domain, f, a)
            })
        }
        Arm::Loop => {
            let options = super::gate_blocks::twm::TwmOptions::from_env()
                .map_err(PlannerError::InvalidConfiguration)?;
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                planner::loop_plan_with_twm(f, a, options)
            })
        }
        Arm::Lean => {
            let options = planner::lean::LeanOptions::from_env()
                .map_err(PlannerError::InvalidConfiguration)?;
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                planner::lean::lean_plan_body(f, a, &options)
            })
        }
        Arm::Composed => planner::scripted_plan(facts, attempt, catalogue, target),
        Arm::Fine => planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
            planner::fine_statement_only_plan_body(f, a)
        }),
        Arm::FineContext => {
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                planner::fine_context_statement_only_plan_body(f, a)
            })
        }
        Arm::Expert2 => {
            planner::scripted_plan_generic(facts, attempt, catalogue, target, |f, a| {
                expert2_plan(selection.domain, f, a)
            })
        }
    }
}

/// Expert block keys for the one selected domain. Experience blocks never
/// enter the runtime model planner's offered set.
pub(crate) fn expert_keys(domain: Domain) -> Vec<CompositionBlockKey> {
    match domain {
        Domain::None => Vec::new(),
        Domain::Django => django::expert::blocks::keys(),
        Domain::Sympy => sympy::expert::blocks::keys(),
        Domain::Sphinx => sphinx::expert::blocks::keys(),
    }
}

/// Every domain variant's own block keys, folded into
/// [`super::gate_blocks::coding_block_catalogue`]'s own key listing test.
/// Most variants are empty; the `expert` variants register their own blocks.
/// See `cosmos/domains/README.md`.
pub(crate) fn keys() -> Vec<CompositionBlockKey> {
    django::expert::blocks::keys()
        .into_iter()
        .chain(django::experience::blocks::keys())
        .chain(sympy::expert::blocks::keys())
        .chain(sympy::experience::blocks::keys())
        .chain(sphinx::expert::blocks::keys())
        .chain(sphinx::experience::blocks::keys())
        .chain(django::expert2::blocks::keys())
        .chain(sympy::expert2::blocks::keys())
        .chain(sphinx::expert2::blocks::keys())
        .collect()
}

/// Registers every domain variant's own blocks into the sealed catalogue.
/// Most variants are empty; the `expert` variants register their own blocks.
/// See `cosmos/domains/README.md`.
///
/// # Errors
/// Returns [`CompositionCatalogueError::DuplicateBlock`] if a later addition
/// names a key already registered elsewhere.
pub(crate) fn register_all(
    catalogue: &mut CompositionCatalogue,
) -> Result<(), CompositionCatalogueError> {
    django::expert::blocks::register_all(catalogue)?;
    django::experience::blocks::register_all(catalogue)?;
    sympy::expert::blocks::register_all(catalogue)?;
    sympy::experience::blocks::register_all(catalogue)?;
    sphinx::expert::blocks::register_all(catalogue)?;
    sphinx::experience::blocks::register_all(catalogue)?;
    django::expert2::blocks::register_all(catalogue)?;
    sympy::expert2::blocks::register_all(catalogue)?;
    sphinx::expert2::blocks::register_all(catalogue)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use polaris_cosmos::composition::GateExpr;
    use polaris_cosmos::evidence::gate_plan_record;

    use super::*;
    use crate::cosmos::gate_blocks::coding_block_catalogue;
    use crate::cosmos::gate_blocks::{
        configurable::{ModelStepConfig, REVIEW_TURN_ALLOWANCE, ToolSubset},
        fine,
    };

    fn target() -> TopologyContractKey {
        crate::cosmos::topology::contract()
            .expect("the coding contract registers cleanly")
            .key()
            .clone()
    }

    fn statement_only_facts() -> PlannerFacts {
        PlannerFacts {
            registration: "test".to_owned(),
            scope_size: 1,
            success_check: "stated-edits",
            objective: "careful",
            latency_budget_ms: None,
            test_runner_available: false,
            reproduce_runner_available: false,
            model_provider_available: false,
            is_question: false,
            is_survey: false,
            has_stated_edits: false,
            statement: "add() subtracts instead of adding: fix the implementation".to_owned(),
            shell_available: true,
        }
    }

    fn attempt() -> RevisionAttempt {
        RevisionAttempt::new(1).expect("one is a valid attempt number")
    }

    #[test]
    fn unknown_domain_names_what_is_accepted() {
        let error = DomainSelection::resolve("djangoo", "generic")
            .expect_err("an unknown domain must be refused");
        let message = error.to_string();
        assert!(message.contains("djangoo"));
        assert!(message.contains("django"));
    }

    #[test]
    fn unknown_arm_names_what_is_accepted() {
        let error = DomainSelection::resolve("django", "expertt")
            .expect_err("an unknown arm must be refused");
        let message = error.to_string();
        assert!(message.contains("expertt"));
        assert!(message.contains("expert"));
    }

    #[test]
    fn an_arm_that_needs_a_domain_is_refused_without_one() {
        for arm in ["generic-notes", "expert", "experience", "composed"] {
            let error = DomainSelection::resolve("none", arm)
                .expect_err("this arm cannot run without a domain");
            assert!(error.to_string().contains(arm));
        }
    }

    #[test]
    fn composed_resolves_only_with_a_selected_domain() {
        for domain in ["django", "sympy", "sphinx"] {
            let selection = DomainSelection::resolve(domain, "composed")
                .expect("composed needs and accepts an expert domain");
            assert_eq!(selection.arm, Arm::Composed);
        }
    }

    #[test]
    fn generic_pairs_with_every_domain_including_none() {
        for domain in ["none", "django", "sympy", "sphinx"] {
            DomainSelection::resolve(domain, "generic")
                .expect("the generic arm never needs a domain");
        }
    }

    /// The generic arm must be unchanged: with domain none or arm generic, the plan is
    /// digest-identical to the unmodified [`planner::scripted_plan`],
    /// whichever domain rides along.
    #[test]
    fn the_generic_arm_reproduces_scripted_plan_for_every_domain() {
        let catalogue = coding_block_catalogue();
        let target = target();
        let facts = statement_only_facts();
        let baseline = planner::scripted_plan(&facts, attempt(), &catalogue, &target)
            .expect("the generic plan builds");
        let baseline_digest = gate_plan_record(&baseline).plan_digest().to_string();

        for domain in [Domain::None, Domain::Django, Domain::Sympy, Domain::Sphinx] {
            let selection = DomainSelection {
                domain,
                arm: Arm::Generic,
            };
            let plan = plan_for_selection(selection, &facts, attempt(), &catalogue, &target)
                .expect("the generic arm builds for every domain");
            assert_eq!(
                gate_plan_record(&plan).plan_digest().to_string(),
                baseline_digest,
                "arm generic must be byte-identical to scripted_plan for {domain}"
            );
        }
    }

    /// Every domain variant must build a plan. Variants differ from the
    /// generic plan once it has notes or a plan of its own, so only the
    /// generic arm is pinned to the generic digest.
    #[test]
    fn every_domain_variant_builds() {
        let catalogue = coding_block_catalogue();
        let target = target();
        let facts = statement_only_facts();
        for domain in [Domain::Django, Domain::Sympy, Domain::Sphinx] {
            for arm in [Arm::GenericNotes, Arm::Expert, Arm::Experience] {
                let selection = DomainSelection { domain, arm };
                plan_for_selection(selection, &facts, attempt(), &catalogue, &target)
                    .unwrap_or_else(|e| panic!("{domain}/{arm} must build: {e:?}"));
            }
        }
    }

    /// The `loop` topology is domain-independent: it pairs only with
    /// [`Domain::None`], the mirror image of [`Arm::GenericNotes`]/
    /// [`Arm::Expert`]/[`Arm::Experience`], which need a domain.
    #[test]
    fn loop_pairs_only_with_domain_none() {
        DomainSelection::resolve("none", "loop").expect("loop runs with no domain selected");
        for domain in ["django", "sympy", "sphinx"] {
            let error = DomainSelection::resolve(domain, "loop")
                .expect_err("loop must be refused alongside a real domain");
            assert!(error.to_string().contains("loop"));
        }
    }

    /// The `loop` arm must actually differ from the generic arm — the whole point of adding it
    /// is a second, comparable composition around the same kind of model
    /// step — and its one-step-plus-conclude plan must still materialise
    /// under the same conservative limits every other shape does.
    #[test]
    fn the_loop_arm_differs_from_generic_and_materialises() {
        let catalogue = coding_block_catalogue();
        let contracts =
            crate::cosmos::gate_blocks::coding_target_contracts().expect("coding contract catalog");
        let target = target();
        let facts = statement_only_facts();

        let generic = plan_for_selection(
            DomainSelection {
                domain: Domain::None,
                arm: Arm::Generic,
            },
            &facts,
            attempt(),
            &catalogue,
            &target,
        )
        .expect("the generic arm builds");
        let loop_plan = plan_for_selection(
            DomainSelection {
                domain: Domain::None,
                arm: Arm::Loop,
            },
            &facts,
            attempt(),
            &catalogue,
            &target,
        )
        .expect("the loop arm builds");
        assert_eq!(
            gate_plan_record(&loop_plan).plan_digest().to_string(),
            "sha256:2bfffff41bced1c29f6a8db90aa55d95e285be25012d4fbd63c3d091efc6368e"
        );
        assert_ne!(
            gate_plan_record(&generic).plan_digest().to_string(),
            gate_plan_record(&loop_plan).plan_digest().to_string(),
            "the loop arm must not be digest-identical to the generic arm's own plan"
        );

        let graph = polaris_cosmos::composition::materialize(
            &loop_plan,
            &catalogue,
            &polaris_cosmos::composition::CompositionLimits::conservative(),
            &contracts,
        )
        .expect("the loop arm's plan materialises under conservative limits");
        assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
    }

    #[test]
    fn lean_arm_materialises_with_host_gated_shell_and_its_own_limits() {
        let selection = DomainSelection::resolve("none", "lean").unwrap();
        assert_eq!(selection.arm.as_str(), "lean");
        for domain in ["django", "sympy", "sphinx"] {
            assert!(DomainSelection::resolve(domain, "lean").is_err());
        }
        let catalogue = coding_block_catalogue();
        let contracts = crate::cosmos::gate_blocks::coding_target_contracts().unwrap();
        for shell in [false, true] {
            let mut facts = statement_only_facts();
            facts.shell_available = shell;
            let options = planner::lean::LeanOptions {
                locate: false,
                // This test isolates shell tool gating in the original block
                // catalogue; the default cascade needs a shell and its blocks.
                cascade: false,
                ..Default::default()
            };
            let (_, configs) = planner::lean::lean_plan_body(&facts, attempt(), &options).unwrap();
            let config = ModelStepConfig::decode(&configs[0].1).unwrap();
            assert_eq!(config.max_turns, 100);
            assert_eq!(config.budget.0, u32::MAX);
            assert!(!config.compact_history);
            assert!(config.strict_turn_cap);
            assert!(
                config
                    .instruction
                    .contains("do not download, install or fetch")
            );
            assert_eq!(config.instruction.contains("project's own tests"), shell);
            let ToolSubset::Only(tools) = config.tool_subset else {
                panic!("tools")
            };
            assert_eq!(tools.iter().any(|tool| tool == "run_command"), shell);
            assert_eq!(tools.iter().any(|tool| tool == "run_python"), shell);
            assert_eq!(
                tools.iter().any(|tool| tool == "find_definition"),
                options.autotest
            );
            assert_eq!(
                tools.iter().any(|tool| tool == "find_references"),
                options.autotest
            );
            let plan =
                planner::scripted_plan_generic(&facts, attempt(), &catalogue, &target(), |f, a| {
                    planner::lean::lean_plan_body(f, a, &options)
                })
                .unwrap();
            let graph = polaris_cosmos::composition::materialize(
                &plan,
                &catalogue,
                &polaris_cosmos::composition::CompositionLimits::conservative(),
                &contracts,
            )
            .unwrap();
            assert!(graph.validate().is_ok());
        }
    }

    #[test]
    fn fine_arm_materialises_and_only_splits_the_initial_proposal() {
        let catalogue = coding_block_catalogue();
        let contracts = crate::cosmos::gate_blocks::coding_target_contracts().expect("contract");
        let target = target();
        DomainSelection::resolve("none", "fine").expect("fine accepts no domain");
        for domain in ["django", "sympy", "sphinx"] {
            assert!(DomainSelection::resolve(domain, "fine").is_err());
        }
        for (runner, shell) in [(true, true), (false, true), (false, false)] {
            let mut facts = statement_only_facts();
            facts.test_runner_available = runner;
            facts.shell_available = shell;
            let baseline = plan_for_selection(
                DomainSelection::default(),
                &facts,
                attempt(),
                &catalogue,
                &target,
            )
            .expect("A builds");
            let fine_plan = plan_for_selection(
                DomainSelection::resolve("none", "fine").expect("fine selection"),
                &facts,
                attempt(),
                &catalogue,
                &target,
            )
            .expect("fine builds");
            let GateExpr::Seq(baseline_steps) = baseline.expression() else {
                panic!("A is a sequence")
            };
            let GateExpr::Seq(fine_steps) = fine_plan.expression() else {
                panic!("fine is a sequence")
            };
            let expected_propose = if shell && !runner {
                fine::propose_conversation_key()
            } else {
                fine::propose_key()
            };
            for (index, expected) in [fine::locate_key(), fine::reproduce_key(), expected_propose]
                .iter()
                .enumerate()
            {
                let GateExpr::Step(step) = &fine_steps[index] else {
                    panic!("stage must be a step")
                };
                assert_eq!(step.block(), expected);
            }
            assert_eq!(
                &fine_steps[3..],
                &baseline_steps[1..],
                "all of A's downstream gates and repairs stay identical"
            );
            let configs = fine_plan.configurations();
            let stage_configs = ["fine-locate", "fine-reproduce", "propose-edit"].map(|id| {
                let id = polaris_cosmos::composition::ConfigId::new(id).expect("config id");
                ModelStepConfig::decode(configs.get(&id).expect("stage config"))
                    .expect("valid config")
            });
            assert_eq!(
                stage_configs
                    .iter()
                    .map(|config| config.max_turns)
                    .collect::<Vec<_>>(),
                [6, 6, 16]
            );
            assert_eq!(
                stage_configs
                    .iter()
                    .map(|config| config.budget.0)
                    .collect::<Vec<_>>(),
                [40_000, 40_000, 120_000]
            );
            assert!(stage_configs.iter().all(|config| {
                config
                    .instruction
                    .contains("Work only with the checkout already in the workspace")
            }));
            assert!(stage_configs[2].review_before_submit);
            let a_config =
                polaris_cosmos::composition::ConfigId::new("propose-edit").expect("config id");
            let a_step = ModelStepConfig::decode(
                baseline
                    .configurations()
                    .get(&a_config)
                    .expect("A proposal"),
            )
            .expect("valid A config");
            assert!(
                stage_configs
                    .iter()
                    .map(|config| u32::from(config.max_turns))
                    .sum::<u32>()
                    <= u32::from(a_step.max_turns)
            );
            assert_eq!(
                stage_configs
                    .iter()
                    .map(|config| config.budget.0)
                    .sum::<u32>(),
                a_step.budget.0
            );
            let max_calls = |config: &ModelStepConfig| {
                u64::from(config.max_turns)
                    + if config.review_before_submit {
                        u64::from(REVIEW_TURN_ALLOWANCE)
                    } else {
                        0
                    }
                    + 1 // one corrective submission after an invalid final answer
            };
            let fine_max_calls: u64 = stage_configs.iter().map(max_calls).sum();
            assert!(fine_max_calls <= max_calls(&a_step));
            let fine_worst_case_tokens: u64 = stage_configs
                .iter()
                .map(|config| u64::from(config.budget.0) * max_calls(config))
                .sum();
            let a_worst_case_tokens = u64::from(a_step.budget.0) * max_calls(&a_step);
            assert!(fine_worst_case_tokens <= a_worst_case_tokens);
            let repair_id = polaris_cosmos::composition::ConfigId::new("propose-edit-repair")
                .expect("repair id");
            for (id, config) in baseline.configurations() {
                if id != &a_config && id != &repair_id {
                    assert_eq!(
                        configs.get(id),
                        Some(config),
                        "A's later-stage configuration changed"
                    );
                }
            }
            if let Some(repair) = configs.get(&repair_id) {
                let f_repair = ModelStepConfig::decode(repair).expect("fine repair config");
                let a_repair = ModelStepConfig::decode(
                    baseline
                        .configurations()
                        .get(&repair_id)
                        .expect("A repair config"),
                )
                .expect("valid A repair config");
                assert_eq!(f_repair.max_turns, a_repair.max_turns);
                assert_eq!(f_repair.budget, a_repair.budget);
                assert_eq!(f_repair.tool_subset, a_repair.tool_subset);
                assert_eq!(f_repair.sees_prior_failure, a_repair.sees_prior_failure);
                assert!(
                    f_repair
                        .instruction
                        .contains("Work only with the checkout already in the workspace")
                );
            }
            let graph = polaris_cosmos::composition::materialize(
                &fine_plan,
                &catalogue,
                &polaris_cosmos::composition::CompositionLimits::conservative(),
                &contracts,
            )
            .expect("fine materialises under conservative limits");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
        }
    }

    #[test]
    fn fine_context_changes_only_the_three_stage_handoff_modes() {
        use crate::cosmos::gate_blocks::configurable::ContextMode;

        let catalogue = coding_block_catalogue();
        let contracts = crate::cosmos::gate_blocks::coding_target_contracts().expect("contract");
        let target = target();
        assert!(DomainSelection::resolve("none", "fine-ctx").is_ok());
        for domain in ["django", "sympy", "sphinx"] {
            assert!(DomainSelection::resolve(domain, "fine-ctx").is_err());
        }
        for (runner, shell) in [(true, true), (false, true), (false, false)] {
            let mut facts = statement_only_facts();
            facts.test_runner_available = runner;
            facts.shell_available = shell;
            let fine = plan_for_selection(
                DomainSelection::resolve("none", "fine").expect("fine arm"),
                &facts,
                attempt(),
                &catalogue,
                &target,
            )
            .expect("fine plan");
            let fine_ctx = plan_for_selection(
                DomainSelection::resolve("none", "fine-ctx").expect("fine-ctx arm"),
                &facts,
                attempt(),
                &catalogue,
                &target,
            )
            .expect("fine-ctx plan");
            assert_eq!(fine.expression(), fine_ctx.expression());
            assert_eq!(fine.configurations().len(), fine_ctx.configurations().len());
            for (id, fine_config) in fine.configurations() {
                let fine_ctx_config = fine_ctx.configurations().get(id).expect("same config key");
                let expected_mode = if id
                    == &polaris_cosmos::composition::ConfigId::new("fine-locate").expect("id")
                {
                    Some(ContextMode::Brief)
                } else if id
                    == &polaris_cosmos::composition::ConfigId::new("fine-reproduce").expect("id")
                    || id
                        == &polaris_cosmos::composition::ConfigId::new("propose-edit").expect("id")
                {
                    Some(ContextMode::Continue)
                } else {
                    None
                };
                if let Some(mode) = expected_mode {
                    let decoded =
                        ModelStepConfig::decode(fine_ctx_config).expect("fine-ctx config");
                    assert_eq!(decoded.context_mode, mode);
                    assert_eq!(
                        decoded
                            .with_context_mode(ContextMode::Fresh)
                            .encode()
                            .expect("encode"),
                        *fine_config,
                        "only the handoff mode may differ for {id:?}"
                    );
                } else {
                    assert_eq!(
                        fine_config, fine_ctx_config,
                        "downstream config changed: {id:?}"
                    );
                }
            }
            let graph = polaris_cosmos::composition::materialize(
                &fine_ctx,
                &catalogue,
                &polaris_cosmos::composition::CompositionLimits::conservative(),
                &contracts,
            )
            .expect("fine-ctx materialises");
            assert!(graph.validate().is_ok(), "{:?}", graph.validate().errors());
            assert_ne!(
                gate_plan_record(&fine).plan_digest().to_string(),
                gate_plan_record(&fine_ctx).plan_digest().to_string()
            );
            if runner && shell {
                assert_eq!(
                    gate_plan_record(&fine).plan_digest().to_string(),
                    "sha256:3ee23738174aa940b2aa5e48f540c39d69240791f67b536affa765c727ba6c60"
                );
                assert_eq!(
                    gate_plan_record(&fine_ctx).plan_digest().to_string(),
                    "sha256:ea3f3540e335b22129435c27bd93a3069cc9fdc2e5359ec01fafed9c8e2ba752"
                );
            }
        }
    }
}
