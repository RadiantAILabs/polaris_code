//! Interactive model and permission settings applied at turn boundaries.
//!
//! # The permission ceiling is enforced here
//!
//! [`SessionSettings`] is the only mutable holder of a session's permission
//! mode: `/permission` writes one, a `/new` or `/resume` transition resets one,
//! and [`prepare_context`] reads exactly this snapshot back to build the policy
//! the turn resolves every tool call against. So an operator-set
//! [`PermissionCeiling`] is enforced at this one place — every write below
//! passes through [`SessionSettings::admit`] — rather than being re-checked at
//! each surface that might raise a posture.
//!
//! [`prepare_context`]: crate::agents::code_agent

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

static APPROVAL_GENERATION: AtomicU64 = AtomicU64::new(1);

use polaris_tui::commands::CommandSpec;

use crate::agents::{PermissionCeiling, PermissionMode};
use crate::config::ModelCatalog;

use super::{CommandError, CommandOutcome, CommandRegistry};

/// The postures `/permission` can select, in the order the picker offers them.
///
/// One list serves three purposes that must not drift apart: parsing the
/// argument, advertising the choices, and wording the "must be one of" error.
/// A ceiling filters this list once and all three follow.
fn selectable_modes() -> [(&'static str, PermissionMode); 4] {
    [
        ("confirm", PermissionMode::Confirm),
        ("auto-edit", PermissionMode::AutoEdit),
        ("auto", PermissionMode::Auto),
        ("plan", PermissionMode::Plan),
    ]
}

/// Immutable settings read by one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSnapshot {
    /// Fully-qualified model id.
    pub model_id: String,
    /// Coding permission mode.
    pub permission_mode: PermissionMode,
}

/// Session settings shared by command handlers and the agent context.
#[derive(Debug, Clone)]
pub struct SessionSettings {
    inner: Arc<RwLock<SettingsSnapshot>>,
    approval_generation: Arc<AtomicU64>,
    /// Shared with every clone and written at most once, so the gate cannot be
    /// escaped by holding a copy of these settings taken before the ceiling was
    /// installed, and cannot be raised by installing a second one.
    ceiling: Arc<OnceLock<PermissionCeiling>>,
    /// The model the most recent turn actually ran on, written by the agent
    /// once it has resolved the swap. Distinct from `inner.model_id`, which is
    /// what was *requested*: a record that names an un-run model is worse than
    /// one that names none, so the host's accounting reads this.
    served: Arc<RwLock<Option<String>>>,
    catalog: Arc<ModelCatalog>,
    apply_at_turn_boundary: bool,
}

impl polaris::system::resource::LocalResource for SessionSettings {}

impl SessionSettings {
    /// Build settings from the active defaults and the run's model catalog.
    ///
    /// `model_id` is the model the session starts on; `catalog` is everything
    /// it may move to, which is what the `/model` chooser offers.
    #[must_use]
    pub fn new(model_id: impl Into<String>, mode: PermissionMode, catalog: &ModelCatalog) -> Self {
        Self {
            inner: Arc::new(RwLock::new(SettingsSnapshot {
                model_id: model_id.into(),
                permission_mode: mode,
            })),
            approval_generation: Arc::new(AtomicU64::new(
                APPROVAL_GENERATION.fetch_add(1, Ordering::SeqCst),
            )),
            ceiling: Arc::new(OnceLock::new()),
            served: Arc::new(RwLock::new(None)),
            catalog: Arc::new(catalog.clone()),
            apply_at_turn_boundary: true,
        }
    }

    /// Hold these settings under an operator-set permission ceiling.
    ///
    /// The active mode is clamped into the ceiling as the ceiling is installed,
    /// so a session can never be *observed* above its ceiling — not even
    /// between construction and the first command. Every later write is refused
    /// or clamped by [`admit`](Self::admit).
    ///
    /// The ceiling is shared with every clone of these settings, including ones
    /// taken before this call, and is written at most once: a session's ceiling
    /// is fixed for the life of the process, so a second install leaves the
    /// first one in force rather than replacing it.
    #[must_use]
    pub fn with_permission_ceiling(self, ceiling: PermissionCeiling) -> Self {
        {
            let mut held = self.inner.write().unwrap_or_else(|err| err.into_inner());
            let clamped = ceiling.clamp(held.permission_mode.clone());
            held.permission_mode = clamped;
        }
        let _ = self.ceiling.set(ceiling);
        self
    }

    /// The ceiling in force, if the operator set one.
    #[must_use]
    pub fn permission_ceiling(&self) -> Option<&PermissionCeiling> {
        self.ceiling.get()
    }

    /// The mode a write of `requested` may actually install: `requested`
    /// itself, or the refusal that names the ceiling standing in its way.
    ///
    /// The one gate every route that raises a posture passes through.
    fn admit(&self, requested: PermissionMode) -> Result<PermissionMode, String> {
        match self.ceiling.get() {
            Some(ceiling) if !ceiling.admits(&requested) => Err(ceiling.refusal(requested.label())),
            _ => Ok(requested),
        }
    }

    pub(crate) fn compatibility_default(model_id: impl Into<String>) -> Self {
        // A host that wired no selection surface pins the session to the model
        // it configured: there is nothing to choose between, and offering a
        // choice the host never installed a command for would be a fiction.
        let model_id = model_id.into();
        let catalog = ModelCatalog::pinned(model_id.clone());
        let mut settings = Self::new(model_id, PermissionMode::Confirm, &catalog);
        settings.apply_at_turn_boundary = false;
        settings
    }

    pub(crate) fn turn_snapshot(&self) -> Option<SettingsSnapshot> {
        self.apply_at_turn_boundary.then(|| self.snapshot())
    }

    /// Identity of the current session approval lifetime. Resetting settings
    /// invalidates remembered grants, including when reopening the same session.
    #[must_use]
    pub fn approval_generation(&self) -> u64 {
        self.approval_generation.load(Ordering::SeqCst)
    }

    /// Current values, atomically cloned for a turn.
    #[must_use]
    pub fn snapshot(&self) -> SettingsSnapshot {
        self.inner
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    /// The models this session may select between.
    #[must_use]
    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    /// Record the model a turn is about to run on.
    ///
    /// Called by the agent once the swap has either succeeded or been refused,
    /// so what the host reports is the model that served the turn rather than
    /// the model somebody asked for.
    pub fn note_served(&self, model_id: &str) {
        let mut served = self.served.write().unwrap_or_else(|err| err.into_inner());
        *served = Some(model_id.to_string());
    }

    /// The model the most recent turn ran on, or `None` before any turn has.
    #[must_use]
    pub fn served_model(&self) -> Option<String> {
        self.served
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    /// Restore the safe interactive defaults for a newly entered session.
    ///
    /// A ceiling stricter than `confirm` lowers the default rather than being
    /// bypassed by it: entering a session must never be a way to come out one
    /// rung higher than the operator allowed.
    pub fn reset(&self, model_id: impl Into<String>) {
        self.approval_generation.store(
            APPROVAL_GENERATION.fetch_add(1, Ordering::SeqCst),
            Ordering::SeqCst,
        );
        let permission_mode = match self.ceiling.get() {
            Some(ceiling) => ceiling.clamp(PermissionMode::Confirm),
            None => PermissionMode::Confirm,
        };
        *self.inner.write().unwrap_or_else(|err| err.into_inner()) = SettingsSnapshot {
            model_id: model_id.into(),
            permission_mode,
        };
        *self.served.write().unwrap_or_else(|err| err.into_inner()) = None;
    }

    /// [`Self::reset`] for a session entered while the user is already on a
    /// chosen model: it stays on that model, so a `/model` choice carries
    /// across `/new` and `/resume` for the life of the process.
    ///
    /// Only the posture is a safety default to fall back to; the model is a
    /// preference, and putting the launch model back would silently undo it.
    pub fn reset_keeping_model(&self) {
        self.reset(self.snapshot().model_id);
    }

    /// The postures `/permission` offers here — every selectable mode, minus
    /// any the ceiling refuses.
    fn offered_modes(&self) -> Vec<&'static str> {
        selectable_modes()
            .into_iter()
            .filter(|(_, mode)| self.admit(mode.clone()).is_ok())
            .map(|(name, _)| name)
            .collect()
    }

    /// The posture after the active one among those `/permission` offers,
    /// wrapping around; the first offered when the active mode is not one of
    /// them. Built from [`offered_modes`](Self::offered_modes), so it follows
    /// the selectable list and the ceiling without a list of its own.
    fn next_offered_mode(&self) -> &'static str {
        let offered = self.offered_modes();
        let active = self.snapshot().permission_mode;
        let next = selectable_modes()
            .iter()
            .find(|(_, mode)| *mode == active)
            .and_then(|(name, _)| offered.iter().position(|o| o == name))
            .map_or(0, |at| (at + 1) % offered.len());
        offered[next]
    }

    /// Register `/model` and `/permission` with finite picker choices.
    ///
    /// `/model`'s choices are the catalog, so the chooser offers what the host
    /// is configured to reach rather than the single model it happens to be on.
    ///
    /// Under a ceiling the `/permission` spec advertises only the postures that
    /// can actually be entered, and says which ceiling narrowed it — so a user
    /// who cannot raise their permissions reads that from the command popup
    /// rather than discovering it by being refused.
    pub fn register(&self, commands: &CommandRegistry) -> Result<(), CommandError> {
        let model_choices = self.catalog.models().to_vec();
        let model_settings = self.clone();
        commands.register_sync(
            CommandSpec::new("model", "Show or select the session model")
                .with_usage("[provider/model]")
                .with_choices(model_choices),
            move |_, args| model_settings.model_command(args),
        )?;

        let offered = self.offered_modes();
        let description = match self.ceiling.get() {
            Some(ceiling) => format!(
                "Show or select the permission mode (ceiling: {})",
                ceiling.label()
            ),
            None => "Show or select the permission mode".to_string(),
        };
        let permission_settings = self.clone();
        commands.register_sync(
            CommandSpec::new("permission", description)
                .with_usage(format!("[{}|cycle]", offered.join("|")))
                .with_choices(offered),
            move |_, args| permission_settings.permission_command(args),
        )
    }

    fn model_command(&self, args: &str) -> CommandOutcome {
        let requested = args.trim();
        if requested.is_empty() {
            let active = self.snapshot().model_id;
            let listed = self
                .catalog
                .models()
                .iter()
                .map(|id| {
                    if *id == active {
                        format!("`{id}` (active)")
                    } else {
                        format!("`{id}`")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            return CommandOutcome::Reply(format!("Available models: {listed}."));
        }
        // Resolved against the catalog, so an unknown name is refused here with
        // the set that would have worked — never carried into a turn to fail as
        // a provider error at the first token.
        let resolved = match self.catalog.resolve_noting(requested) {
            Ok(resolved) => resolved,
            Err(err) => {
                return CommandOutcome::Error(format!("{err}; the active model was not changed"));
            }
        };
        self.inner
            .write()
            .unwrap_or_else(|err| err.into_inner())
            .model_id = resolved.id.clone();
        CommandOutcome::Reply(match resolved.note() {
            Some(note) => format!("{note}. Model set for the next turn."),
            None => format!("Model set to `{}` for the next turn.", resolved.id),
        })
    }

    fn permission_command(&self, args: &str) -> CommandOutcome {
        let requested = args.trim().to_ascii_lowercase();
        if requested.is_empty() {
            let active = format!(
                "Active permission mode: `{}`.",
                self.snapshot().permission_mode.label()
            );
            return CommandOutcome::Reply(match self.ceiling.get() {
                Some(ceiling) => format!(
                    "{active} Permission ceiling `{}` is in force for this process; \
                     `/permission` accepts {} and refuses anything above the ceiling.",
                    ceiling.label(),
                    offered_list(&self.offered_modes())
                ),
                None => active,
            });
        }
        // `cycle` stands for the next posture on offer. Resolving it to a mode
        // word here sends it through the same lookup and ceiling check below,
        // which is what a key bound to `/permission cycle` relies on.
        let requested = if requested == "cycle" {
            self.next_offered_mode().to_string()
        } else {
            requested
        };
        let Some((_, mode)) = selectable_modes()
            .into_iter()
            .find(|(name, _)| *name == requested)
        else {
            return CommandOutcome::Error(format!(
                "permission mode must be {}; the active mode was not changed",
                offered_list(&self.offered_modes())
            ));
        };
        // The ceiling is enforced before the write, not reported after it: a
        // refused escalation must leave the session in the posture it had.
        let mode = match self.admit(mode) {
            Ok(mode) => mode,
            Err(refusal) => {
                return CommandOutcome::Error(format!(
                    "{refusal}; the active mode was not changed"
                ));
            }
        };
        self.inner
            .write()
            .unwrap_or_else(|err| err.into_inner())
            .permission_mode = mode;
        CommandOutcome::Reply(format!(
            "Permission mode set to `{requested}` for the next turn."
        ))
    }
}

/// Render offered postures as `` `a`, `b`, or `c` `` for a one-line message.
///
/// The empty case cannot arise — a ceiling always admits its own rung — but is
/// spelled rather than panicked on.
fn offered_list(offered: &[&'static str]) -> String {
    let quoted: Vec<String> = offered.iter().map(|name| format!("`{name}`")).collect();
    match quoted.split_last() {
        None => "none".to_string(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, or {last}", rest.join(", ")),
    }
}
