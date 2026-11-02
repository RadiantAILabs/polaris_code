//! The per-turn policy snapshot — the read seam every control decision consults.
//!
//! The snapshot uses [`PolicySnapshot::defaults`], and every decision point
//! routes through it. [`PolicySnapshot::load`] is the policy-as-data seam and
//! loads the installed permission-rule capability.

use std::collections::HashMap;
use std::num::NonZeroU32;

use polaris::system::resource::LocalResource;
use polaris::tools::ToolPermission;
use polaris_ontrace::oxigraph::OxigraphStore;

use crate::tools::{AttendedApproval, FILE_WRITE_TOOLS, ToolEffect, ToolPolicyRegistry};

/// Default cap on messages retained in the projected live window.
pub const DEFAULT_MAX_MESSAGES: usize = 50;
/// Default cap on the estimated token mass of the projected live window.
pub const DEFAULT_MAX_WINDOW_TOKENS: usize = 120_000;
/// Default token threshold above which [`CacheAwareWindow`] starts compacting
/// (stubbing the oldest tool-result bodies). Below the `max_tokens` ceiling so
/// compaction relieves pressure before the window would otherwise be force-slid.
///
/// [`CacheAwareWindow`]: crate::agents::context::CacheAwareWindow
pub const DEFAULT_COMPACT_AFTER_TOKENS: usize = 60_000;
/// Default number of tool-result bodies stubbed per compaction jump.
pub const DEFAULT_COMPACT_BATCH: usize = 8;
/// Default number of most-recent messages compaction never touches.
pub const DEFAULT_COMPACT_KEEP_RECENT: usize = 12;
/// Second-stage summary threshold after cheap tool-result elision.
pub const DEFAULT_SUMMARIZE_AFTER_TOKENS: usize = 90_000;
/// Maximum output allowance for a summary checkpoint.
pub const DEFAULT_SUMMARY_OUTPUT_TOKENS: NonZeroU32 = NonZeroU32::new(2_048).unwrap();

/// How `prepare_context` assembles the window. (Active in v1 — see
/// [`PolicySnapshot::defaults`].)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrievalPolicy {
    /// Optional cap on messages retained in the live window.
    pub max_messages: Option<usize>,
    /// Optional cap on the estimated token mass of the live window.
    pub max_tokens: Option<usize>,
    /// Token threshold above which the cache-aware strategy compacts old tool
    /// results. `None` disables compaction.
    pub compact_after_tokens: Option<usize>,
    /// Tool-result bodies stubbed per compaction jump.
    pub compact_batch: usize,
    /// Most-recent messages compaction leaves intact, so fresh observations stay
    /// readable.
    pub compact_keep_recent: usize,
    /// Token threshold after elision for graph-based summary checkpoints.
    /// `None` disables automatic summaries.
    pub summarize_after_tokens: Option<usize>,
    /// Maximum output tokens reserved and requested for a summary call.
    pub summary_output_tokens: NonZeroU32,
}

impl Default for RetrievalPolicy {
    /// No caps, no compaction and no automatic summaries, with a summary
    /// output allowance of [`DEFAULT_SUMMARY_OUTPUT_TOKENS`]. A summary needs
    /// some allowance to be requested at all, so zero is not representable.
    fn default() -> Self {
        Self {
            max_messages: None,
            max_tokens: None,
            compact_after_tokens: None,
            compact_batch: 0,
            compact_keep_recent: 0,
            summarize_after_tokens: None,
            summary_output_tokens: DEFAULT_SUMMARY_OUTPUT_TOKENS,
        }
    }
}

/// How a turn's tool palette is selected from the registry's exposed tools.
///
/// Models the three legal selection states as one enum so the ambiguous
/// "no allow-list yet not allow-all" combination cannot be represented.
/// Composes with the registry's static exposure (the registry
/// decides what *may* be advertised; this decides what *is* advertised this
/// turn) and with the orthogonal [`ToolPalettePolicy::denied`] denylist.
#[derive(Debug, Clone, Default)]
pub enum PaletteMode {
    /// Offer the full exposed registry. The behavior-preserving v1 default.
    #[default]
    All,
    /// Offer only the named tools (still intersected with exposure and
    /// [`denied`](ToolPalettePolicy::denied)); an empty list offers nothing.
    ///
    /// This mode narrows the palette per goal. Because its names are reified
    /// into the trajectory `ABox` as RDF literals, the writer escapes them; see
    /// [`Decision::record`](crate::agents::trajectory::Decision::record).
    Only(Vec<String>),
    /// Offer no tools this turn — an explicit pure-reasoning turn.
    None,
}

impl PaletteMode {
    /// Short label naming how the palette was selected, for reified provenance.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            PaletteMode::All => "full-registry",
            PaletteMode::Only(_) => "selected",
            PaletteMode::None => "none",
        }
    }
}

/// Which tools are offered to the model for a turn.
///
/// The per-turn tool-selection lever (the agent narrowing its own toolset): a
/// [`PaletteMode`] plus an orthogonal denylist. v1 defaults to
/// [`PaletteMode::All`] with no denials, so the seam is wired end-to-end
/// without changing behavior.
#[derive(Debug, Clone, Default)]
pub struct ToolPalettePolicy {
    /// How this turn's palette is selected from the exposed registry.
    pub mode: PaletteMode,
    /// Tool names to withhold regardless of `mode` — applied on top of whatever
    /// `mode` selects.
    pub denied: Vec<String>,
}

impl ToolPalettePolicy {
    /// Whether a tool named `name` is offered to the model this turn.
    ///
    /// A tool is offered iff it is not in [`denied`](Self::denied) and the
    /// [`mode`](Self::mode) admits it: [`All`](PaletteMode::All) admits every
    /// tool, [`Only`](PaletteMode::Only) admits names in its list, and
    /// [`None`](PaletteMode::None) admits nothing.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_code::agents::{PaletteMode, ToolPalettePolicy};
    ///
    /// // The default offers the whole (exposed) registry.
    /// assert!(ToolPalettePolicy::default().includes("read_file"));
    ///
    /// // An allow-list, minus the denylist, narrows the turn.
    /// let narrowed = ToolPalettePolicy {
    ///     mode: PaletteMode::Only(vec!["read_file".into(), "search".into()]),
    ///     denied: vec!["search".into()],
    /// };
    /// assert!(narrowed.includes("read_file"));
    /// assert!(!narrowed.includes("search")); // denied wins
    /// assert!(!narrowed.includes("edit_file")); // not in the allow-list
    ///
    /// // An explicit pure-reasoning turn offers nothing.
    /// let none = ToolPalettePolicy { mode: PaletteMode::None, denied: vec![] };
    /// assert!(!none.includes("read_file"));
    /// ```
    #[must_use]
    pub fn includes(&self, name: &str) -> bool {
        if self.denied.iter().any(|d| d == name) {
            return false;
        }
        match &self.mode {
            PaletteMode::All => true,
            PaletteMode::Only(names) => names.iter().any(|s| s == name),
            PaletteMode::None => false,
        }
    }
}

/// Who answers a call that deterministic policy leaves at
/// [`Confirm`](ToolPermission::Confirm).
///
/// This is the whole difference between an attended, an automatically reviewed
/// and an unattended session, so it is one value rather than a pair of flags
/// that could disagree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Approver {
    /// A person, asked by the host. `confirm`, `auto-edit` and `plan`.
    #[default]
    Human,
    /// The graph's automatic reviewer first, then a person whenever it asks,
    /// cannot decide or is not allowed to see the call. `auto`.
    Reviewer,
    /// Nobody: a would-be confirmation is a refusal. Unattended runs.
    Nobody,
}

/// Context-sensitive permission overrides resolved before execution.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    /// Who answers a confirmation that policy leaves open.
    pub approver: Approver,
    /// Per-tool overrides; absent → fall back to the registry's permission.
    pub overrides: HashMap<String, ToolPermission>,
    /// Immutable permission files installed by the permission-rules capability.
    pub rules: Option<super::permission_rules::PermissionRules>,
    /// Policy facts the tool capabilities published about their tools.
    pub tools: ToolPolicyRegistry,
}

impl PermissionPolicy {
    /// Whether nobody can answer a confirmation, so it must be refused.
    #[must_use]
    pub fn is_unattended(&self) -> bool {
        self.approver == Approver::Nobody
    }

    /// Whether the automatic reviewer sees confirmations before a person does.
    #[must_use]
    pub fn uses_reviewer(&self) -> bool {
        self.approver == Approver::Reviewer
    }

    /// The `plan` posture: nothing that writes files or runs commands.
    fn is_plan(&self) -> bool {
        matches!(
            (
                self.overrides.get("edit_file"),
                self.overrides.get("run_command"),
            ),
            (Some(ToolPermission::Deny), Some(ToolPermission::Deny))
        )
    }

    /// Resolve the effective permission for `tool`, falling back to
    /// `registry_default` when no override applies.
    ///
    /// A tool whose capability declared [`AttendedApproval::Human`] is resolved
    /// from those facts instead: refused when the registry or an override
    /// refuses it, asked in every attended mode however it was overridden, and
    /// run unattended only on an explicit `Allow` override. `plan` is the one
    /// exception to the asking: it runs a tool declared
    /// [`ToolEffect::ReadOnly`] and refuses one declared
    /// [`ToolEffect::Mutating`].
    #[must_use]
    pub fn resolve(&self, tool: &str, registry_default: ToolPermission) -> ToolPermission {
        let overridden = self.overrides.get(tool).copied();
        let declared = self.tools.get(tool);
        if declared.attended() == AttendedApproval::Human {
            if registry_default == ToolPermission::Deny || overridden == Some(ToolPermission::Deny)
            {
                return ToolPermission::Deny;
            }
            if self.is_unattended() {
                return if overridden == Some(ToolPermission::Allow) {
                    ToolPermission::Allow
                } else {
                    ToolPermission::Deny
                };
            }
            return match (self.is_plan(), declared.effect()) {
                (true, Some(ToolEffect::ReadOnly)) => ToolPermission::Allow,
                (true, Some(ToolEffect::Mutating)) => ToolPermission::Deny,
                _ => ToolPermission::Confirm,
            };
        }
        let resolved = overridden.unwrap_or(registry_default);
        if self.is_unattended() && resolved == ToolPermission::Confirm {
            ToolPermission::Deny
        } else {
            resolved
        }
    }

    /// Argument-aware first pass before an optional reviewer and human approval.
    /// Hard mode denials and ceilings precede rules; deny > ask > allow, with
    /// session grants considered only when no restrictive rule matched.
    ///
    /// The decision also records whether the automatic reviewer may settle it
    /// ([`AuthorizationDecision::review`](super::permission_rules::AuthorizationDecision::review)). It may not when a rule asks (an
    /// explicit `ask` rule, or a project `allow` rule that is not yet trusted)
    /// or when the tool's capability declared [`AttendedApproval::Human`]; those
    /// calls reach a person, and neither allow rules nor session grants apply
    /// to the latter.
    #[must_use]
    pub fn resolve_call(
        &self,
        call: &super::permission_rules::PermissionCall<'_>,
        grants: &super::permission_rules::SessionGrants,
    ) -> super::permission_rules::AuthorizationDecision {
        use super::permission_rules::{
            AuthorizationDecision, ReviewEligibility, RuleDecision, ceiling_permission,
        };
        let base = self.resolve(call.tool, call.registry_default);
        let human_only_tool = self.tools.get(call.tool).attended() == AttendedApproval::Human;
        let matched = self
            .rules
            .as_ref()
            .and_then(|rules| rules.resolve(call.tool, call.args));
        let ceiling = call.ceiling.map(|ceiling| {
            let mut held = ceiling.mode().permission_policy();
            held.tools = self.tools.clone();
            held.resolve(call.tool, call.registry_default)
        });
        let mut result = AuthorizationDecision {
            permission: base,
            source: format!("permission mode: {}", self.mode_label()),
            remember: !self.is_unattended(),
            session_grant: false,
            review: if human_only_tool {
                ReviewEligibility::HumanOnly
            } else {
                ReviewEligibility::Eligible
            },
        };
        if base == ToolPermission::Deny {
            result.remember = false;
            return result;
        }
        match matched {
            Some((RuleDecision::Deny, source)) => {
                result.permission = ToolPermission::Deny;
                result.source = source;
                result.remember = false;
            }
            Some((RuleDecision::Ask, source)) => {
                result.permission = if self.is_unattended() {
                    ToolPermission::Deny
                } else {
                    ToolPermission::Confirm
                };
                result.source = source;
                result.remember = false;
                result.review = ReviewEligibility::HumanOnly;
            }
            Some((RuleDecision::Allow, source)) if !self.is_unattended() && !human_only_tool => {
                result.permission = ToolPermission::Allow;
                result.source = source;
                result.session_grant =
                    grants.allows(call.generation, call.tool, call.args, call.root);
            }
            _ if !self.is_unattended()
                && !human_only_tool
                && grants.allows(call.generation, call.tool, call.args, call.root) =>
            {
                result.permission = ToolPermission::Allow;
                result.source = "human session command grant".into();
                result.session_grant = true;
            }
            _ => {}
        }
        let bounded = ceiling_permission(result.permission, ceiling);
        if bounded != result.permission
            && let Some(held) = call.ceiling
        {
            result.source = format!("{} permission ceiling", held.label());
        }
        result.permission = bounded;
        if matches!(
            ceiling,
            Some(ToolPermission::Confirm | ToolPermission::Deny)
        ) {
            result.remember = false;
        }
        result
    }

    /// Stable label for trajectory provenance.
    #[must_use]
    pub fn mode_label(&self) -> &'static str {
        match self.approver {
            Approver::Nobody => return "unattended",
            Approver::Reviewer => return "auto",
            Approver::Human => {}
        }
        match (
            self.overrides.get("edit_file"),
            self.overrides.get("run_command"),
        ) {
            (Some(ToolPermission::Allow), Some(ToolPermission::Confirm)) => "auto-edit",
            _ if self.is_plan() => "plan",
            _ => "confirm",
        }
    }
}

/// Named recipe for resolving tool permissions for one session or run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PermissionMode {
    /// Ask before tools whose registry or host policy requires confirmation.
    #[default]
    Confirm,
    /// Allow file edits without asking while shell commands still require approval.
    AutoEdit,
    /// Apply edits and review remaining confirmations with a model before dispatch.
    Auto,
    /// Refuse mutating tools while leaving read-only tools available.
    Plan,
    /// Apply immutable construction-time grants without ever asking a human.
    Unattended(UnattendedGrants),
}

/// Construction-time grants for one unattended run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnattendedGrants {
    /// Whether the file-writing tools — `edit_file` and `create_file` — may
    /// run without confirmation. One grant covers both: creating a file is
    /// gated on exactly the footing of editing one.
    pub edit_file: bool,
    /// Shell command patterns allowed by this run's existing shell policy.
    pub run_command_patterns: Vec<String>,
}

impl PermissionMode {
    /// Stable command and provenance label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::AutoEdit => "auto-edit",
            Self::Auto => "auto",
            Self::Plan => "plan",
            Self::Unattended(_) => "unattended",
        }
    }

    /// Where this mode sits on the **attended restriction ladder** — the one
    /// ordering [`PermissionCeiling`] is defined over.
    ///
    /// The ladder is `plan` (0) < `confirm` (1) < `auto-edit` (2). That order
    /// is the pointwise one on [`ToolPermission`] (`Deny` < `Confirm` <
    /// `Allow`) over the two tools a mode governs, as
    /// [`permission_parts`](Self::permission_parts) builds them:
    ///
    /// | mode | `edit_file` | `run_command` |
    /// |---|---|---|
    /// | `plan` | `Deny` | `Deny` |
    /// | `confirm` | registry default (`Confirm`) | `Confirm` |
    /// | `auto-edit` | `Allow` | `Confirm` |
    ///
    /// Each rung up relaxes exactly one tool's gate and tightens none, so "at
    /// most as permissive as" is a *total* order on the three. Asking a human
    /// counts as a restriction, not as a permission: a mode that asks sits
    /// strictly below the mode that simply does it.
    ///
    /// [`Unattended`](Self::Unattended) has **no rung**, which is why this
    /// returns an `Option`. `Auto` also has no rung: its reviewer capability
    /// is admitted explicitly by `PermissionCeiling`, never inferred from
    /// these deterministic tool gates. Unattended is not "above auto-edit": an unattended policy
    /// turns every would-be confirmation into a refusal
    /// ([`Approver::Nobody`]), so
    /// against `confirm` it is *stricter* on every tool it holds no grant for
    /// and *more permissive* on the ones it does — `--allow-command 'cargo *'`
    /// runs `cargo` with nobody asked, which `auto-edit` would still confirm.
    /// Two unattended runs with different grants are incomparable with each
    /// other for the same reason. Unattended is a different shape rather than a
    /// higher rung, so a ceiling refuses to order it instead of guessing.
    fn attended_rung(&self) -> Option<u8> {
        match self {
            Self::Plan => Some(0),
            Self::Confirm => Some(1),
            Self::AutoEdit => Some(2),
            Self::Auto | Self::Unattended(_) => None,
        }
    }

    /// Produce the per-tool overrides represented by this mode.
    #[must_use]
    pub fn tool_overrides(&self) -> HashMap<String, ToolPermission> {
        self.permission_parts().0
    }

    /// Produce the existing permission-policy seam for this mode.
    ///
    /// The overrides and the approver come from the same match arm so a future
    /// mode cannot configure one without the other.
    #[must_use]
    pub fn permission_policy(&self) -> PermissionPolicy {
        let (overrides, approver) = self.permission_parts();
        PermissionPolicy {
            overrides,
            approver,
            ..PermissionPolicy::default()
        }
    }

    fn permission_parts(&self) -> (HashMap<String, ToolPermission>, Approver) {
        let (edit, shell) = match self {
            Self::Confirm => (None, ToolPermission::Confirm),
            Self::AutoEdit | Self::Auto => (Some(ToolPermission::Allow), ToolPermission::Confirm),
            Self::Plan => (Some(ToolPermission::Deny), ToolPermission::Deny),
            Self::Unattended(grants) => (
                Some(if grants.edit_file {
                    ToolPermission::Allow
                } else {
                    ToolPermission::Deny
                }),
                if grants.run_command_patterns.is_empty() {
                    ToolPermission::Deny
                } else {
                    ToolPermission::Allow
                },
            ),
        };
        // One decision, applied to every name that can run a shell command, so
        // a mode cannot answer differently for the foreground and background
        // routes to the same command. A new mode that wanted to diverge would
        // have to say so here rather than by forgetting an entry.
        let mut overrides: HashMap<String, ToolPermission> = SHELL_EXECUTION_TOOLS
            .iter()
            .map(|name| ((*name).to_string(), shell))
            .collect();
        // The file-writing tools ride one gate the same way: whatever a mode
        // decides for `edit_file` it decides for `create_file`, so a creation
        // is never looser or stricter than an edit. `Confirm` sets neither and
        // leaves both on the registry's confirming default.
        if let Some(edit) = edit {
            overrides.extend(
                FILE_WRITE_TOOLS
                    .iter()
                    .map(|name| ((*name).to_string(), edit)),
            );
        }
        let approver = match self {
            Self::Auto => Approver::Reviewer,
            Self::Unattended(_) => Approver::Nobody,
            Self::Confirm | Self::AutoEdit | Self::Plan => Approver::Human,
        };
        (overrides, approver)
    }
}

/// Every tool name that can put a shell command on this machine.
///
/// A command that runs in the background is the same command, so
/// [`PermissionMode`] resolves one verdict and writes it against all of these:
/// backgrounding is a scheduling choice, never a permission one. `stop_task`
/// is here too — it signals a process group, which is a shell effect even
/// though it starts nothing.
pub const SHELL_EXECUTION_TOOLS: [&str; 3] = ["run_command", "run_in_background", "stop_task"];

/// "At most as permissive as", over the attended restriction ladder documented
/// on [`PermissionMode::attended_rung`]: `Plan < Confirm < AutoEdit`.
///
/// The order is deliberately *partial*.
/// [`Unattended`](PermissionMode::Unattended) is incomparable with all three —
/// and with a differently-granted unattended run — so `partial_cmp` answers
/// `None` there rather than inventing a rung. Every comparison that decides
/// whether a posture may be entered therefore has to handle "cannot be
/// ordered", and [`PermissionCeiling::admits`] answers it with a refusal.
///
/// `Auto` is also incomparable with the human-approval ladder. Its reviewer
/// capability is admitted explicitly by `PermissionCeiling::admits`.
///
/// `Some(Ordering::Equal)` is returned exactly when the two modes are equal, so
/// this stays consistent with the derived [`PartialEq`].
impl PartialOrd for PermissionMode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self.attended_rung(), other.attended_rung()) {
            (Some(left), Some(right)) => Some(left.cmp(&right)),
            _ => (self == other).then_some(std::cmp::Ordering::Equal),
        }
    }
}

/// The maximum permission posture a process may reach, fixed before its session
/// starts.
///
/// The operator names one at launch and nothing reachable from inside the
/// session — a command, a setting, a prompt, a tool — may exceed it. A ceiling
/// holds a rung of the attended ladder
/// ([`PermissionMode::attended_rung`]) or the explicit `Auto` capability.
/// [`PermissionMode::Unattended`] cannot be
/// one, because a ceiling has to order every posture against itself and an
/// unattended posture orders against nothing.
///
/// This is one invariant rather than a policy engine: [`admits`](Self::admits)
/// answers the only question there is, and the single mutable holder of a
/// session's mode — [`SessionSettings`](crate::terminal::SessionSettings) —
/// enforces the answer on every route that writes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionCeiling(PermissionMode);

impl PermissionCeiling {
    /// Build a ceiling at a human-approval rung or the explicit `Auto`
    /// capability. Unattended modes cannot be ceilings.
    ///
    /// # Examples
    ///
    /// ```
    /// use polaris_code::agents::{PermissionCeiling, PermissionMode, UnattendedGrants};
    ///
    /// let ceiling = PermissionCeiling::new(PermissionMode::Confirm).expect("confirm is a rung");
    /// assert!(ceiling.admits(&PermissionMode::Plan));
    /// assert!(!ceiling.admits(&PermissionMode::AutoEdit));
    ///
    /// // An unattended posture is a different shape, not a higher rung.
    /// assert!(PermissionCeiling::new(PermissionMode::Unattended(UnattendedGrants::default())).is_none());
    /// assert!(!ceiling.admits(&PermissionMode::Unattended(UnattendedGrants::default())));
    /// ```
    #[must_use]
    pub fn new(mode: PermissionMode) -> Option<Self> {
        (mode.attended_rung().is_some() || mode == PermissionMode::Auto).then_some(Self(mode))
    }

    /// The posture at the ceiling.
    #[must_use]
    pub fn mode(&self) -> &PermissionMode {
        &self.0
    }

    /// The ceiling's label, in the same vocabulary `/permission` and the
    /// trajectory already use.
    #[must_use]
    pub fn label(&self) -> &'static str {
        self.0.label()
    }

    /// Whether a session may run in `mode` under this ceiling.
    ///
    /// `Auto` requires an `Auto` ceiling; that ceiling also admits the three
    /// human-approval modes. Other ceilings use the original ladder.
    /// Unattended modes are always refused.
    #[must_use]
    pub fn admits(&self, mode: &PermissionMode) -> bool {
        // Automated command approval requires its own explicit capability.
        // AutoEdit still promises that a human approves commands.
        if matches!(mode, PermissionMode::Auto) {
            return self.0 == PermissionMode::Auto;
        }
        if self.0 == PermissionMode::Auto {
            return mode.attended_rung().is_some();
        }
        matches!(
            mode.partial_cmp(&self.0),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        )
    }

    /// `mode` when this ceiling admits it, the ceiling's own posture otherwise.
    ///
    /// The only caller-visible effect is downward for anything on the ladder:
    /// the startup default and the per-session reset both ask for `confirm` and
    /// get `plan` under a `plan` ceiling. It is *not* a way to bound an
    /// unattended run — an unattended posture is incomparable, so clamping one
    /// would hand back the attended ceiling and put a human back in a loop that
    /// has none. That is why the host refuses `--permission-ceiling` together
    /// with `--print` instead of clamping it.
    #[must_use]
    pub fn clamp(&self, mode: PermissionMode) -> PermissionMode {
        if self.admits(&mode) {
            mode
        } else {
            self.0.clone()
        }
    }

    /// The shared clause refusing `requested`, naming the ceiling in force.
    ///
    /// The ceiling owns the words so that every route that can refuse — the
    /// startup flags, `/permission` — says the same thing about the same
    /// invariant, and each caller appends only its own consequence.
    #[must_use]
    pub fn refusal(&self, requested: &str) -> String {
        format!(
            "`{requested}` exceeds the `{}` permission ceiling this process was started with",
            self.label()
        )
    }
}

/// How transient model-call failures are retried.
///
/// Only a failure the provider is likely to recover from is retried (see
/// [`FailureKind::is_transient`](super::failure::FailureKind::is_transient));
/// the deciding logic is [`RetryPolicy::next_step`], which the agent graph's
/// retry stage consults after every failed attempt.
///
/// # Defaults
///
/// | Field | Default | Meaning |
/// |-------|---------|---------|
/// | [`max_attempts`](Self::max_attempts) | 5 | The first call plus four retries. |
/// | [`base_delay`](Self::base_delay) | 1 s | The wait before the first retry; it doubles for each later one. |
/// | [`max_delay`](Self::max_delay) | 60 s | The longest single wait, computed or requested by the provider. |
/// | [`attempt_timeout`](Self::attempt_timeout) | 300 s | The longest an attempt may go without a provider event. |
///
/// With these numbers the computed waits are about 1, 2, 4 and 8 s, each
/// scaled by a random factor between 0.75 and 1.25. The worst case is a
/// provider that goes silent on every attempt and asks for the longest wait
/// in between: five silent attempts of 300 s and four waits of 60 s, which is
/// [`RetryPolicy::worst_case_wait`], 1,740 s (29 minutes). Lower
/// `attempt_timeout` or `max_attempts` to bound an unattended run more
/// tightly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum attempts for a transient LLM failure, the first included
    /// (>= 1). `1` turns retrying off.
    pub max_attempts: u32,
    /// The wait before the first retry. Each later retry waits twice as long as
    /// the one before, with jitter.
    pub base_delay: std::time::Duration,
    /// The longest a single wait may be, whether computed or requested by the
    /// provider. A provider that asks for longer ends the retrying instead.
    pub max_delay: std::time::Duration,
    /// How long one attempt may go without the provider sending anything — to
    /// the first event, and between events after that — before it is abandoned
    /// as a timeout.
    pub attempt_timeout: std::time::Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            base_delay: std::time::Duration::from_secs(1),
            max_delay: std::time::Duration::from_secs(60),
            attempt_timeout: std::time::Duration::from_secs(300),
        }
    }
}

/// Snapshot of all control-decision policy for one turn. A [`LocalResource`]
/// built once in the agent's `setup` and read by every decision point.
#[derive(Debug, Clone, Default)]
pub struct PolicySnapshot {
    /// Retrieval / context-assembly policy.
    pub retrieval: RetrievalPolicy,
    /// Tool-palette policy.
    pub tool_palette: ToolPalettePolicy,
    /// Permission-resolution policy.
    pub permission: PermissionPolicy,
    /// Retry policy.
    pub retry: RetryPolicy,
}

impl PolicySnapshot {
    /// Hardcoded sane defaults — the v1 policy. The retrieval budget is
    /// **active**: the projected window is bounded by both a message-count cap
    /// and an estimated token cap.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            retrieval: RetrievalPolicy {
                max_messages: Some(DEFAULT_MAX_MESSAGES),
                max_tokens: Some(DEFAULT_MAX_WINDOW_TOKENS),
                compact_after_tokens: Some(DEFAULT_COMPACT_AFTER_TOKENS),
                compact_batch: DEFAULT_COMPACT_BATCH,
                compact_keep_recent: DEFAULT_COMPACT_KEEP_RECENT,
                summarize_after_tokens: Some(DEFAULT_SUMMARIZE_AFTER_TOKENS),
                summary_output_tokens: DEFAULT_SUMMARY_OUTPUT_TOKENS,
            },
            ..Default::default()
        }
    }

    /// Load permission data from the installed capability. Store and intent
    /// remain available for future intent-specific policy selection.
    pub async fn load(
        store: &OxigraphStore,
        intent: Option<&str>,
        rules: &super::permission_rules::PermissionRules,
    ) -> Self {
        let _ = (store, intent);
        let mut snapshot = Self::defaults();
        snapshot.permission.rules = Some(rules.clone());
        snapshot
    }
}

impl LocalResource for PolicySnapshot {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_defaults_to_offering_everything() {
        let palette = ToolPalettePolicy::default();
        assert!(palette.includes("read_file"));
        assert!(palette.includes("edit_file"));
    }

    #[test]
    fn palette_denylist_withholds_named_tools() {
        let palette = ToolPalettePolicy {
            denied: vec!["edit_file".into()],
            ..Default::default()
        };
        assert!(palette.includes("read_file"));
        assert!(!palette.includes("edit_file"));
    }

    #[test]
    fn palette_allowlist_offers_only_selected_minus_denied() {
        let palette = ToolPalettePolicy {
            mode: PaletteMode::Only(vec!["read_file".into(), "search".into()]),
            denied: vec!["search".into()],
        };
        // In the allow-list and not denied.
        assert!(palette.includes("read_file"));
        // In the allow-list but denied → withheld.
        assert!(!palette.includes("search"));
        // Not in the allow-list → withheld.
        assert!(!palette.includes("edit_file"));
    }

    #[test]
    fn palette_none_offers_no_tools() {
        let palette = ToolPalettePolicy {
            mode: PaletteMode::None,
            denied: Vec::new(),
        };
        assert!(!palette.includes("read_file"));
        assert!(!palette.includes("edit_file"));
    }

    #[test]
    fn palette_empty_allowlist_offers_no_tools() {
        let palette = ToolPalettePolicy {
            mode: PaletteMode::Only(Vec::new()),
            denied: Vec::new(),
        };
        assert!(!palette.includes("read_file"));
    }

    #[test]
    fn palette_mode_labels_match_selection() {
        assert_eq!(PaletteMode::All.label(), "full-registry");
        assert_eq!(
            PaletteMode::Only(vec!["read_file".into()]).label(),
            "selected"
        );
        assert_eq!(PaletteMode::None.label(), "none");
    }

    #[test]
    fn permission_override_takes_precedence() {
        let mut policy = PermissionPolicy::default();
        assert_eq!(
            policy.resolve("edit_file", ToolPermission::Confirm),
            ToolPermission::Confirm
        );
        policy
            .overrides
            .insert("edit_file".into(), ToolPermission::Allow);
        assert_eq!(
            policy.resolve("edit_file", ToolPermission::Confirm),
            ToolPermission::Allow
        );
    }

    /// The ordering the ceiling is defined over, asserted as the ladder rather
    /// than as three isolated pairs: `plan` is the most restrictive posture and
    /// each step up only relaxes.
    #[test]
    fn the_attended_ladder_runs_plan_below_confirm_below_auto_edit() {
        assert!(PermissionMode::Plan < PermissionMode::Confirm);
        assert!(PermissionMode::Confirm < PermissionMode::AutoEdit);
        assert!(PermissionMode::Plan < PermissionMode::AutoEdit);
        assert!(PermissionMode::Confirm <= PermissionMode::Confirm);
    }

    /// The incomparability the ceiling refuses on. An unattended posture orders
    /// against nothing — not against the ladder, and not against another
    /// unattended run whose grants differ.
    #[test]
    fn an_unattended_posture_orders_against_nothing_but_itself() {
        let unattended = PermissionMode::Unattended(UnattendedGrants::default());
        for attended in [
            PermissionMode::Plan,
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
        ] {
            assert_eq!(
                unattended.partial_cmp(&attended),
                None,
                "`unattended` must not be ordered against `{}`",
                attended.label()
            );
            assert_eq!(attended.partial_cmp(&unattended), None);
        }

        let granted = PermissionMode::Unattended(UnattendedGrants {
            edit_file: true,
            run_command_patterns: vec!["cargo *".into()],
        });
        assert_eq!(unattended.partial_cmp(&granted), None);
        assert_eq!(
            granted.partial_cmp(&granted.clone()),
            Some(std::cmp::Ordering::Equal),
            "the same posture is comparable with itself, keeping PartialOrd consistent with PartialEq"
        );
    }

    #[test]
    fn a_ceiling_admits_its_own_rung_and_everything_below_it() {
        let ceiling = PermissionCeiling::new(PermissionMode::Confirm).expect("confirm is a rung");
        assert!(ceiling.admits(&PermissionMode::Plan));
        assert!(ceiling.admits(&PermissionMode::Confirm));
        assert!(!ceiling.admits(&PermissionMode::AutoEdit));
        assert!(!ceiling.admits(&PermissionMode::Unattended(UnattendedGrants::default())));
    }

    /// A ceiling has to be able to order every posture against itself, so the
    /// one posture that orders against nothing cannot be spelled as one.
    #[test]
    fn an_unattended_posture_cannot_be_a_ceiling() {
        assert!(
            PermissionCeiling::new(PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec!["cargo *".into()],
            }))
            .is_none()
        );
    }

    #[test]
    fn clamping_lowers_and_never_raises() {
        let plan = PermissionCeiling::new(PermissionMode::Plan).expect("plan is a rung");
        assert_eq!(plan.clamp(PermissionMode::Confirm), PermissionMode::Plan);
        assert_eq!(plan.clamp(PermissionMode::Plan), PermissionMode::Plan);

        let auto_edit =
            PermissionCeiling::new(PermissionMode::AutoEdit).expect("auto-edit is a rung");
        assert_eq!(
            auto_edit.clamp(PermissionMode::Confirm),
            PermissionMode::Confirm,
            "a ceiling above the requested mode must leave it alone"
        );
    }

    #[test]
    fn a_refusal_names_the_ceiling_and_the_posture_it_refused() {
        let refusal = PermissionCeiling::new(PermissionMode::Plan)
            .expect("plan is a rung")
            .refusal("auto-edit");
        assert!(refusal.contains("auto-edit"), "{refusal}");
        assert!(refusal.contains("plan"), "{refusal}");
        assert!(refusal.contains("ceiling"), "{refusal}");
    }

    #[test]
    fn every_mode_resolves_the_file_writing_tools_together() {
        for mode in [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Plan,
            PermissionMode::Unattended(UnattendedGrants::default()),
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: Vec::new(),
            }),
        ] {
            let policy = mode.permission_policy();
            let resolved: Vec<_> = FILE_WRITE_TOOLS
                .iter()
                .map(|tool| policy.resolve(tool, ToolPermission::Confirm))
                .collect();
            assert!(
                resolved.windows(2).all(|pair| pair[0] == pair[1]),
                "{} must gate {FILE_WRITE_TOOLS:?} identically, got {resolved:?}",
                mode.label()
            );
        }
    }

    #[test]
    fn unattended_fails_closed_for_future_confirm_gated_tools() {
        let policy = PermissionMode::Unattended(UnattendedGrants::default()).permission_policy();
        assert_eq!(
            policy.resolve("future_mutating_tool", ToolPermission::Confirm),
            ToolPermission::Deny
        );
        assert_eq!(
            policy.resolve("read_file", ToolPermission::Allow),
            ToolPermission::Allow
        );
    }
}
