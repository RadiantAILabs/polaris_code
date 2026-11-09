//! Settings a run reads from environment variables rather than flags, each
//! validated once at startup so a typo refuses the run before any turn starts.

use std::path::PathBuf;

use crate::cosmos::composition::CompositionPolicy;
use crate::cosmos::domains::{Arm, DomainSelection};
use crate::cosmos::planner::GatePlannerMode;

/// The environment variable that sets the estimated USD ceiling on one coding
/// task.
pub(super) const TASK_MAX_COST_VAR: &str = "POLARIS_CODE_MAX_COST_USD";

/// The environment variable that keeps the activated project runtime in shell
/// tools.
pub(super) const TESTENV_VAR: &str = "POLARIS_CODE_TESTENV";

/// The environment variable that pins this whole run to one registered
/// topology, for every accepted coding task that names none of its own.
pub(super) const TOPOLOGY_PIN_VAR: &str = "POLARIS_CODE_TOPOLOGY_PIN";

/// Resolve the operator's run-wide topology pin against the sealed catalog
/// this run just built, from outside the binary and with no rebuild —
/// `POLARIS_CODE_TOPOLOGY_PIN` read the same way `POLARIS_CODE_GRAPH_SEARCH`
/// already is (see `graph_search` below).
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing selects exactly as it always has, byte for byte.
///
/// Anything else has to name one of `known`, the sealed catalog's own
/// registration ids (the same set `--list-topologies` and `/topologies`
/// show). An unrecognised name is refused here, at startup, before any turn
/// runs — never accepted and silently left for a later turn's selection point
/// to fail on, and never quietly substituted with the selector's own default,
/// which would make a whole benchmark sweep measure the same unpinned run
/// three times and report it as three.
pub(super) fn resolve_topology_pin(
    raw: Option<&str>,
    known: &[String],
) -> Result<Option<String>, String> {
    let Some(name) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if known.iter().any(|candidate| candidate == name) {
        Ok(Some(name.to_owned()))
    } else {
        Err(format!(
            "{TOPOLOGY_PIN_VAR} `{name}` is not a registered topology; known topologies are {} \
             (see --list-topologies)",
            known.join(", ")
        ))
    }
}

/// The environment variable that selects the finite-composition policy every
/// accepted coding task's selected topology composes under.
pub(super) const COMPOSITION_POLICY_VAR: &str = "POLARIS_CODE_COMPOSITION_POLICY";

/// Resolve the operator's run-wide composition policy from outside the
/// binary, read and validated once here — the same pattern
/// [`resolve_topology_pin`] follows for its own env var, and for the same
/// reason: a typo must refuse at startup, naming the values it accepts,
/// rather than being discovered deep inside the first accepted coding task's
/// own selection point
/// ([`composition::CompositionPolicyError::InvalidAnswer`]), which ends the
/// whole session (`DriverStep::Stop`) instead of the process never starting
/// one.
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing selects the deterministic policy exactly as it always
/// has. Anything else has to be one of [`CompositionPolicy::parse`]'s two
/// accepted spellings, `deterministic` or `model`.
pub(super) fn resolve_composition_policy(
    raw: Option<&str>,
) -> Result<Option<CompositionPolicy>, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    CompositionPolicy::parse(value)
        .map(Some)
        .map_err(|error| format!("{COMPOSITION_POLICY_VAR} {error}"))
}

/// The environment variable that selects which gate planner assembles a
/// task-specific plan when the composition policy above is `gates`.
pub(super) const GATE_PLANNER_VAR: &str = "POLARIS_CODE_GATE_PLANNER";

/// Resolve the operator's gate planner mode the same way
/// [`resolve_composition_policy`] resolves its own env var: validated once
/// here, before any turn, rather than discovered deep inside the first
/// accepted coding task's gate-plan revision loop.
///
/// Unset, or set to whitespace, resolves to `Ok(None)` — a run that
/// configures nothing uses the scripted planner exactly as it always has.
/// Anything else has to be one of [`GatePlannerMode::parse`]'s two accepted
/// spellings, `scripted` or `model`. Meaningless unless the composition
/// policy is also `gates`, but validated unconditionally: a typo refuses at
/// startup regardless of which policy is active, rather than only surfacing
/// once an operator also happens to set the other variable.
pub(super) fn resolve_gate_planner_mode(
    raw: Option<&str>,
) -> Result<Option<GatePlannerMode>, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    GatePlannerMode::parse(value)
        .map(Some)
        .map_err(|error| format!("{GATE_PLANNER_VAR} {error}"))
}

/// Select the scripted gates path when lean is named without a separate
/// composition setting. The harness sends its deterministic baseline even
/// for a lean run, so the arm also overrides that baseline to gates.
pub(super) fn lean_composition_policy(
    selection: Option<DomainSelection>,
    policy: Option<CompositionPolicy>,
    planner: Option<GatePlannerMode>,
) -> Result<Option<CompositionPolicy>, String> {
    if !selection.is_some_and(|selection| selection.arm == Arm::Lean) {
        return Ok(policy);
    }
    if policy == Some(CompositionPolicy::Model) {
        return Err("arm lean requires scripted gates, not model composition".to_owned());
    }
    if planner == Some(GatePlannerMode::Model) {
        return Err("arm lean requires POLARIS_CODE_GATE_PLANNER=scripted".to_owned());
    }
    Ok(Some(CompositionPolicy::Gates))
}

/// The environment variable that selects the domain plan/notes a gate-plan
/// turn's statement-only edit shape uses.
pub(super) const DOMAIN_VAR: &str = "POLARIS_CODE_DOMAIN";

/// The environment variable that selects which arm (plan/notes treatment)
/// the domain above uses.
pub(super) const ARM_VAR: &str = "POLARIS_CODE_ARM";

/// Resolve the operator's domain/arm selection from outside the binary,
/// following the same startup-validation discipline
/// [`resolve_composition_policy`] does: a typo, or an arm requested without a
/// domain, refuses at startup naming what is accepted, rather than being
/// discovered per turn deep inside gate-plan composition.
///
/// Unset for both, resolves to `Ok(None)` — a run that configures neither
/// installs no global, and every turn runs the unmodified generic plan
/// exactly as it did before domain plans existed. Setting only one of the
/// two still validates the pair, defaulting the other to its own baseline
/// (`none`/`generic`) first: `POLARIS_CODE_ARM=expert` alone, with no domain
/// named, is exactly as much a configuration error as naming an unknown
/// domain would be.
pub(super) fn resolve_domain_selection(
    domain_raw: Option<&str>,
    arm_raw: Option<&str>,
) -> Result<Option<DomainSelection>, String> {
    let domain = domain_raw.map(str::trim).filter(|value| !value.is_empty());
    let arm = arm_raw.map(str::trim).filter(|value| !value.is_empty());
    if domain.is_none() && arm.is_none() {
        return Ok(None);
    }
    DomainSelection::resolve(domain.unwrap_or("none"), arm.unwrap_or("generic"))
        .map(Some)
        .map_err(|error| format!("{DOMAIN_VAR}/{ARM_VAR} {error}"))
}
/// Resolve the ontology-pack directory: `POLARIS_CODE_ONTOLOGY_DIR` if it names
/// an existing directory, else `None`.
pub(super) fn ontology_dir() -> Option<PathBuf> {
    let dir = std::env::var("POLARIS_CODE_ONTOLOGY_DIR").ok()?;
    let p = PathBuf::from(dir);
    p.is_dir().then_some(p)
}

/// Resolve an on-by-default boolean env gate: `0`/`false`/`off` (case- and
/// whitespace-insensitive) disable it; unset or anything else leaves it on.
/// Takes the `env::var` result so the predicate is pure and unit-testable.
pub(super) fn env_flag_enabled(var: Result<String, std::env::VarError>) -> bool {
    var.map(|s| {
        !matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        )
    })
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::super::cli::parse as cli;
    use super::*;

    /// A run that configures nothing pins nothing — `select_and_run` sees no
    /// `OperatorTopologyPin` at all, byte for byte as it did before this
    /// variable existed.
    #[test]
    fn an_unset_topology_pin_resolves_to_no_pin() {
        let known = vec!["code.direct-edit".to_owned()];
        assert_eq!(resolve_topology_pin(None, &known), Ok(None));
        assert_eq!(resolve_topology_pin(Some(""), &known), Ok(None));
        assert_eq!(resolve_topology_pin(Some("   "), &known), Ok(None));
    }

    /// A name the sealed catalog actually holds is accepted, trimmed of
    /// incidental whitespace.
    #[test]
    fn a_known_topology_pin_is_accepted() {
        let known = vec![
            "code.direct-edit".to_owned(),
            "code.test-gated-edit".to_owned(),
        ];
        assert_eq!(
            resolve_topology_pin(Some("code.direct-edit"), &known),
            Ok(Some("code.direct-edit".to_owned()))
        );
        assert_eq!(
            resolve_topology_pin(Some(" code.test-gated-edit "), &known),
            Ok(Some("code.test-gated-edit".to_owned())),
        );
    }

    /// An unrecognised id is refused, naming what is registered — never
    /// silently substituted with the selector's own default, which would make
    /// a benchmark sweep measure the same unpinned run under two names.
    #[test]
    fn an_unknown_topology_pin_is_refused_and_names_what_is_registered() {
        let known = vec![
            "code.direct-edit".to_owned(),
            "code.test-gated-edit".to_owned(),
        ];
        let error = resolve_topology_pin(Some("code.no-such-thing"), &known)
            .expect_err("an unknown pin must be refused, not defaulted");
        assert!(error.contains(TOPOLOGY_PIN_VAR), "{error}");
        assert!(error.contains("code.no-such-thing"), "{error}");
        assert!(error.contains("code.direct-edit"), "{error}");
        assert!(error.contains("code.test-gated-edit"), "{error}");
    }

    /// A run that configures nothing installs no `CompositionPolicy` global —
    /// `read_composition_policy` sees no resource at all, byte for byte as it
    /// did before this variable existed.
    #[test]
    fn an_unset_composition_policy_resolves_to_no_override() {
        assert_eq!(resolve_composition_policy(None), Ok(None));
        assert_eq!(resolve_composition_policy(Some("")), Ok(None));
        assert_eq!(resolve_composition_policy(Some("   ")), Ok(None));
    }

    #[test]
    fn a_known_composition_policy_is_accepted() {
        assert_eq!(
            resolve_composition_policy(Some("deterministic")),
            Ok(Some(CompositionPolicy::Deterministic))
        );
        assert_eq!(
            resolve_composition_policy(Some(" model ")),
            Ok(Some(CompositionPolicy::Model))
        );
    }

    /// An unrecognised value is refused, naming the accepted values — never
    /// silently defaulted to the deterministic policy, which would make a
    /// benchmark sweep measure the same unintended policy under two names.
    #[test]
    fn an_unknown_composition_policy_is_refused_and_names_what_is_accepted() {
        let error = resolve_composition_policy(Some("Model"))
            .expect_err("an unknown policy must be refused, not defaulted");
        assert!(error.contains(COMPOSITION_POLICY_VAR), "{error}");
        assert!(error.contains("Model"), "{error}");
        assert!(error.contains("deterministic"), "{error}");
        assert!(error.contains("model"), "{error}");
    }

    /// A run that configures nothing installs no `GatePlannerMode` global —
    /// `read_gate_planner_mode` sees no resource at all, and the scripted
    /// planner runs exactly as it did before this variable existed.
    #[test]
    fn an_unset_gate_planner_mode_resolves_to_no_override() {
        assert_eq!(resolve_gate_planner_mode(None), Ok(None));
        assert_eq!(resolve_gate_planner_mode(Some("")), Ok(None));
        assert_eq!(resolve_gate_planner_mode(Some("   ")), Ok(None));
    }

    #[test]
    fn a_known_gate_planner_mode_is_accepted() {
        assert_eq!(
            resolve_gate_planner_mode(Some("scripted")),
            Ok(Some(GatePlannerMode::Scripted))
        );
        assert_eq!(
            resolve_gate_planner_mode(Some(" model ")),
            Ok(Some(GatePlannerMode::Model))
        );
    }

    /// An unrecognised value is refused, naming the accepted values — never
    /// silently defaulted to the scripted planner, which would make a
    /// benchmark sweep measure the same unintended planner under two names.
    #[test]
    fn an_unknown_gate_planner_mode_is_refused_and_names_what_is_accepted() {
        let error = resolve_gate_planner_mode(Some("Model"))
            .expect_err("an unknown mode must be refused, not defaulted");
        assert!(error.contains(GATE_PLANNER_VAR), "{error}");
        assert!(error.contains("Model"), "{error}");
        assert!(error.contains("scripted"), "{error}");
        assert!(error.contains("model"), "{error}");
    }

    /// A run that configures neither variable installs no `DomainSelection`
    /// global — `read_domain_selection` sees no resource at all, and every
    /// turn runs the unmodified generic plan exactly as it did before domain
    /// plans existed.
    #[test]
    fn an_unset_domain_selection_resolves_to_no_override() {
        assert_eq!(resolve_domain_selection(None, None), Ok(None));
        assert_eq!(resolve_domain_selection(Some(""), Some("  ")), Ok(None));
    }

    #[test]
    fn a_known_domain_and_arm_are_accepted() {
        assert_eq!(
            resolve_domain_selection(Some("django"), Some("expert")),
            Ok(Some(DomainSelection {
                domain: crate::cosmos::domains::Domain::Django,
                arm: Arm::Expert,
            }))
        );
    }

    #[test]
    fn lean_selects_scripted_gates_without_extra_environment() {
        let lean = resolve_domain_selection(None, Some("lean")).unwrap();
        assert_eq!(
            lean_composition_policy(lean, None, None),
            Ok(Some(CompositionPolicy::Gates))
        );
        assert_eq!(
            lean_composition_policy(lean, Some(CompositionPolicy::Deterministic), None),
            Ok(Some(CompositionPolicy::Gates))
        );
        assert!(lean_composition_policy(lean, Some(CompositionPolicy::Model), None).is_err());
        assert!(lean_composition_policy(lean, None, Some(GatePlannerMode::Model)).is_err());
        assert_eq!(lean_composition_policy(None, None, None), Ok(None));
    }

    /// Setting only the arm still validates the pair against the domain's
    /// own default (`none`), which refuses exactly as an unknown value would.
    #[test]
    fn an_arm_without_a_domain_is_refused_and_names_the_problem() {
        let error = resolve_domain_selection(None, Some("expert"))
            .expect_err("expert needs a domain other than none");
        assert!(error.contains(DOMAIN_VAR), "{error}");
        assert!(error.contains(ARM_VAR), "{error}");
        assert!(error.contains("expert"), "{error}");
    }

    #[test]
    fn an_unknown_domain_is_refused_and_names_what_is_accepted() {
        let error = resolve_domain_selection(Some("djangoo"), None)
            .expect_err("an unknown domain must be refused, not defaulted");
        assert!(error.contains("djangoo"), "{error}");
        assert!(error.contains("django"), "{error}");
    }

    #[test]
    fn lean_is_accepted_from_cli_and_env_resolution() {
        let args = cli(&["--arm", "lean"]);
        assert_eq!(args.arm.as_deref(), Some("lean"));
        assert_eq!(
            resolve_domain_selection(None, args.arm.as_deref())
                .unwrap()
                .unwrap()
                .arm,
            Arm::Lean
        );
        assert_eq!(
            resolve_domain_selection(None, Some("lean"))
                .unwrap()
                .unwrap()
                .arm,
            Arm::Lean
        );
        assert!(resolve_domain_selection(Some("django"), Some("lean")).is_err());
    }

    /// The `loop` topology is a single model conversation and is the mirror image of `expert`/
    /// `experience`/`generic-notes`: it needs *no* domain, so it is accepted
    /// with `none` (including the unset default) and refused with a real one.
    #[test]
    fn the_loop_arm_requires_domain_none() {
        assert_eq!(
            resolve_domain_selection(Some("none"), Some("loop")),
            Ok(Some(DomainSelection {
                domain: crate::cosmos::domains::Domain::None,
                arm: Arm::Loop,
            }))
        );
        assert_eq!(
            resolve_domain_selection(None, Some("loop")),
            Ok(Some(DomainSelection {
                domain: crate::cosmos::domains::Domain::None,
                arm: Arm::Loop,
            }))
        );
        let error = resolve_domain_selection(Some("django"), Some("loop"))
            .expect_err("loop must be refused alongside a real domain");
        assert!(error.contains("loop"), "{error}");
    }

    #[test]
    fn the_fine_arm_requires_domain_none() {
        assert_eq!(
            resolve_domain_selection(Some("none"), Some("fine")),
            Ok(Some(DomainSelection {
                domain: crate::cosmos::domains::Domain::None,
                arm: Arm::Fine,
            }))
        );
        let error = resolve_domain_selection(Some("django"), Some("fine"))
            .expect_err("fine must be refused alongside a real domain");
        assert!(error.contains("fine"), "{error}");
    }

    #[test]
    fn env_flag_enabled_defaults_on_and_honors_disable_words() {
        use std::env::VarError;
        // Unset → on.
        assert!(env_flag_enabled(Err(VarError::NotPresent)));
        // Disable words, case- and whitespace-insensitive.
        for off in ["0", "false", "off", "OFF", " Off "] {
            assert!(!env_flag_enabled(Ok(off.into())), "{off:?} should disable");
        }
        // Anything else → on.
        for on in ["1", "true", "on", "yes", "anything"] {
            assert!(env_flag_enabled(Ok(on.into())), "{on:?} should enable");
        }
    }
}
