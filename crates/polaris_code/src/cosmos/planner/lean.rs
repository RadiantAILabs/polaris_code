//! The lean arm's extension seam: env options enter here, before plan construction.

use super::*;

/// Lean systems and their independent feature switches.
#[derive(Debug, Clone, PartialEq)]
pub struct LeanOptions {
    /// Batch old tool results into stubs.
    pub trim: bool,
    /// Run relevant tests after successful writes, once per model reply.
    pub autotest: bool,
    /// Prepend cheap-model location findings.
    pub locate: bool,
    /// Ask the editor to verify the location report by reading its named lines.
    pub check_locate: bool,
    /// Keep the model's tool choice stable on the final request.
    pub stable_tool_choice: bool,
    /// Model identity for the optional location step.
    pub locate_model: Option<String>,
    /// Continue a small loop only after a failed system check.
    pub cascade: bool,
    /// Per-task estimated dollar ceiling.
    pub max_cost_usd: Option<f64>,
    /// Model calls allowed, including locate and the final answer; at most 100.
    pub max_turns: u16,
    /// Typed working memory (see `twm`). `None` (the default) leaves the
    /// editing step without working memory.
    pub twm: Option<twm::TwmOptions>,
}

impl Default for LeanOptions {
    fn default() -> Self {
        Self {
            trim: true,
            autotest: true,
            locate: true,
            check_locate: true,
            stable_tool_choice: true,
            locate_model: None,
            cascade: true,
            max_cost_usd: Some(1.50),
            max_turns: 100,
            twm: None,
        }
    }
}

impl LeanOptions {
    /// Reads only lean's named switches and the explicit global turn ceiling.
    /// Call only for arm `lean`; other arms must ignore these variables.
    ///
    /// # Errors
    /// Rejects malformed booleans, nonpositive turn ceilings, and invalid costs.
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Pure parsing seam for hosts and tests, without mutating process env.
    ///
    /// # Errors
    /// Same validation as [`Self::from_env`].
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let boolean = |key, default| match get(key).as_deref().map(str::trim) {
            None | Some("") => Ok(default),
            Some("0") => Ok(false),
            Some("1") => Ok(true),
            _ => Err(format!("{key} must be 0 or 1")),
        };
        let mut options = Self {
            trim: boolean("POLARIS_CODE_LEAN_TRIM", true)?,
            autotest: boolean("POLARIS_CODE_LEAN_AUTOTEST", true)?,
            check_locate: boolean("POLARIS_CODE_LEAN_CHECK_LOCATE", true)?,
            stable_tool_choice: boolean("POLARIS_CODE_LEAN_STABLE_TOOL_CHOICE", true)?,
            locate: match get("POLARIS_CODE_LEAN_LOCATE").as_deref().map(str::trim) {
                None | Some("") | Some("1") => true,
                Some("0") => false,
                _ => return Err("POLARIS_CODE_LEAN_LOCATE must be 0 or 1".to_owned()),
            },
            cascade: boolean("POLARIS_CODE_LEAN_CASCADE", true)?,
            locate_model: get("POLARIS_CODE_LEAN_LOCATE_MODEL")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty()),
            twm: twm::TwmOptions::from_lookup(&get)?,
            ..Self::default()
        };
        if let Some(raw) = get("POLARIS_CODE_MAX_TURNS").filter(|s| !s.trim().is_empty()) {
            let limit = raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|v| *v > 0)
                .ok_or("POLARIS_CODE_MAX_TURNS must be a positive integer")?;
            options.max_turns = limit.min(100) as u16;
        }
        if let Some(raw) = get("POLARIS_CODE_MAX_COST_USD").filter(|s| !s.trim().is_empty()) {
            let cost = raw
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or("POLARIS_CODE_MAX_COST_USD must be finite and nonnegative")?;
            options.max_cost_usd = (cost > 0.0).then_some(cost);
        }
        Ok(options)
    }
}

/// Builds lean's location, editing, and optional system-check cascade.
/// Shell availability controls both the tool list and the system check.
pub(crate) fn lean_plan_body(
    facts: &PlannerFacts,
    _attempt: RevisionAttempt,
    options: &LeanOptions,
) -> StatementPlanBody {
    if options.cascade && !facts.shell_available {
        return Err(PlannerError::InvalidConfiguration(
            "lean cascade needs a policy-allowed Python shell".to_owned(),
        ));
    }
    let total = options.max_turns;
    let locate_turns = if options.locate { 3 } else { 0 };
    if total <= locate_turns || (options.cascade && total <= locate_turns + 1) {
        return Err(PlannerError::InvalidConfiguration(
            "lean needs enough turns for locate and at least one editing turn (two with cascade)"
                .to_owned(),
        ));
    }
    let editing_total = total - locate_turns;
    let tier_one = if options.cascade {
        editing_total.saturating_sub(1).min(18)
    } else {
        editing_total
    };
    let mut tools = vec!["read_file", "list_dir", "search", "edit_file"];
    if options.autotest {
        tools.extend(["find_definition", "find_references"]);
    }
    let tests = if facts.shell_available {
        tools.extend(["run_python", "run_command"]);
        " You can run the project's own tests with run_command."
    } else {
        ""
    };
    let autotest = if options.autotest && facts.shell_available {
        " Writes automatically run a related test subset when recognised; inspect the appended result."
    } else {
        ""
    };
    let mut instruction = format!(
        "Fix this issue by editing the files in the workspace directly:\n{}\n\n\
         Use list_dir, search and read_file to find what to change, then edit_file to make \
         the change — it writes immediately.{tests}{autotest} When you are done, call submit_answer \
         with a short summary of what you changed. Work only with the checkout already in \
         the workspace: do not download, install or fetch another version of the project \
         or its upstream changes. Installing a missing third-party dependency is fine.",
        facts.statement
    );
    if options.cascade {
        instruction.push_str(" Before finishing, use edit_file to replace the placeholder in .polaris/lean-repro.py with a short Python assertion. It must fail on the original behavior and pass after your change.");
    }
    let mut config = ModelStepConfig::new(
        "lean".to_owned(),
        instruction,
        ToolSubset::Only(tools.into_iter().map(str::to_owned).collect()),
        // Per-request, not cumulative. The provider's own context/output limits
        // remain authoritative; this sentinel adds no earlier token stop.
        TokenBudget(u32::MAX),
        ModelStepOutputKind::Answer,
    )
    .with_max_turns(tier_one);
    config.trim = if options.trim {
        TrimPolicy::Batched
    } else {
        TrimPolicy::None
    };
    config.compact_history = false;
    config.strict_turn_cap = true;
    config.stable_tool_choice = options.stable_tool_choice;
    config.autotest = options.autotest;
    config.twm = options.twm;
    let id = ConfigId::new("lean")?;
    let (mut steps, mut configs) = if options.locate {
        let (expression, configs) = locate_then_model_step(
            &facts.statement,
            configurable::model_direct_edit_key(),
            &config.encode()?,
            options.check_locate,
            options.stable_tool_choice,
        )?;
        let GateExpr::Seq(steps) = expression else {
            unreachable!("location is a sequence")
        };
        (steps, configs)
    } else {
        (
            vec![step_expr(
                "lean",
                configurable::model_direct_edit_key(),
                id.clone(),
            )?],
            vec![(id, config.encode()?)],
        )
    };
    if options.cascade {
        let tier_two_id = ConfigId::new("lean-continue")?;
        let mut tier_two = config.clone();
        tier_two.max_turns = total;
        tier_two.context_mode = ContextMode::Continue;
        configs.push((tier_two_id.clone(), tier_two.encode()?));
        let begin = step_expr(
            "cascade-begin",
            super::super::gate_blocks::cascade::begin_key(),
            tier_two_id.clone(),
        )?;
        steps.insert(0, begin);
        steps.push(step_expr(
            "cascade-check",
            super::super::gate_blocks::cascade::check_key(),
            no_config_id()?,
        )?);
        steps.push(GateExpr::Choose {
            predicate: super::super::gate_blocks::predicates::cascade_passed_key(),
            when_true: Box::new(step_expr(
                "cascade-pass",
                super::super::gate_blocks::cascade::conclude_passed_key(),
                no_config_id()?,
            )?),
            when_false: Box::new(GateExpr::Seq(vec![
                step_expr(
                    "lean-continue",
                    super::super::gate_blocks::cascade::continue_key(),
                    tier_two_id,
                )?,
                step_expr(
                    "conclude",
                    configurable::conclude_unverified_key(),
                    no_config_id()?,
                )?,
            ])),
        });
        return Ok((GateExpr::Seq(steps), configs));
    }
    steps.push(step_expr(
        "conclude",
        configurable::conclude_unverified_key(),
        no_config_id()?,
    )?);
    Ok((GateExpr::Seq(steps), configs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_to_location_and_respect_an_explicit_lower_ceiling() {
        assert_eq!(
            LeanOptions::from_lookup(|_| None).unwrap(),
            LeanOptions::default()
        );
        for (value, expected) in [("17", 17), ("100", 100), ("250", 100)] {
            let options = LeanOptions::from_lookup(|key| {
                (key == "POLARIS_CODE_MAX_TURNS").then(|| value.to_owned())
            })
            .unwrap();
            assert_eq!(options.max_turns, expected);
        }
        for flag in [
            "TRIM",
            "AUTOTEST",
            "LOCATE",
            "CASCADE",
            "CHECK_LOCATE",
            "STABLE_TOOL_CHOICE",
        ] {
            let name = format!("POLARIS_CODE_LEAN_{flag}");
            assert!(
                LeanOptions::from_lookup(|key| (key == name).then(|| "yes".to_owned())).is_err()
            );
        }
        let options = LeanOptions::from_lookup(|key| match key {
            "POLARIS_CODE_LEAN_TRIM"
            | "POLARIS_CODE_LEAN_AUTOTEST"
            | "POLARIS_CODE_LEAN_LOCATE"
            | "POLARIS_CODE_LEAN_CASCADE" => Some("1".into()),
            "POLARIS_CODE_LEAN_LOCATE_MODEL" => Some("mock/cheap".into()),
            "POLARIS_CODE_MAX_COST_USD" => Some("0.5".into()),
            _ => None,
        })
        .unwrap();
        assert!(options.trim && options.autotest && options.locate && options.cascade);
        assert_eq!(options.locate_model.as_deref(), Some("mock/cheap"));
        assert_eq!(options.max_cost_usd, Some(0.5));
        for flag in ["TRIM", "LOCATE", "CASCADE"] {
            let name = format!("POLARIS_CODE_LEAN_{flag}");
            let disabled =
                LeanOptions::from_lookup(|key| (key == name).then(|| "0".to_owned())).unwrap();
            assert!(!match flag {
                "TRIM" => disabled.trim,
                "LOCATE" => disabled.locate,
                _ => disabled.cascade,
            });
        }
        let uncapped = LeanOptions::from_lookup(|key| {
            (key == "POLARIS_CODE_MAX_COST_USD").then(|| "0".to_owned())
        })
        .unwrap();
        assert_eq!(uncapped.max_cost_usd, None);
    }

    #[test]
    fn lean_locate_instruction_and_final_choice_are_switchable() {
        let facts = PlannerFacts {
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
            statement: "fix add".to_owned(),
            shell_available: true,
        };
        let attempt = RevisionAttempt::new(1).unwrap();
        let options = LeanOptions::default();
        let (_, configs) = lean_plan_body(&facts, attempt, &options).unwrap();
        let decoded: Vec<_> = configs
            .iter()
            .filter_map(|(_, value)| ModelStepConfig::decode(value).ok())
            .collect();
        assert!(
            decoded
                .iter()
                .any(|step| { step.role == "cheap-locate" && step.stable_tool_choice })
        );
        assert!(decoded.iter().any(|step| {
            step.role == "lean"
                && step.stable_tool_choice
                && step.instruction.contains("Read the report's named lines")
        }));

        let options = LeanOptions {
            check_locate: false,
            stable_tool_choice: false,
            ..options
        };
        let (_, configs) = lean_plan_body(&facts, attempt, &options).unwrap();
        let decoded: Vec<_> = configs
            .iter()
            .filter_map(|(_, value)| ModelStepConfig::decode(value).ok())
            .collect();
        assert!(decoded.iter().all(|step| !step.stable_tool_choice));
        assert!(decoded.iter().any(|step| {
            step.role == "lean" && step.instruction.contains("report may be wrong")
        }));
    }
}
