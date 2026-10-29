//! Host-owned follow-up queue and safe-boundary steering state.
//!
//! The queue is also a containment boundary. It holds [`ContainedPrompt`]s,
//! so every [`TurnInputHandle`] mutator has to resolve a submission's `@`
//! mentions against the bound working directory before the queue will take
//! it, and a refusal comes back to the composer while the user's draft is
//! still there. Nothing downstream — neither `receive_user_input` nor the
//! safe-boundary drain — needs a check of its own, because there is no way to
//! put an unchecked prompt where they read from.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use polaris::system::resource::LocalResource;
use polaris_tui::panels::conversation::{TurnInputHandle, TurnInputMode, TurnInputReceipt};
use tokio::sync::Notify;

use super::mentions::{ContainedPrompt, MentionRejected};

/// One submission as the composer made it: the text, plus the handles of the
/// attachments its chips carried.
///
/// The two travel together from here on. Splitting them would let a prompt
/// reach the model in a turn where its image did not, which is the one failure
/// the composer's submission guard exists to prevent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnPrompt {
    /// The text the user typed, with its `@` mentions already resolved
    /// against the working directory the queue is bound to.
    pub text: ContainedPrompt,
    /// Host attachment handles, in chip order, redeemable once against the
    /// session's [`AttachmentStore`](super::AttachmentStore).
    pub attachments: Vec<String>,
}

impl TurnPrompt {
    /// A checked prompt with nothing attached.
    #[must_use]
    pub fn new(text: ContainedPrompt) -> Self {
        Self {
            text,
            attachments: Vec::new(),
        }
    }
}

/// A prompt equals a bare string only when it *is* a bare string.
///
/// Attachments are part of what was submitted, so a prompt carrying one is not
/// the same submission as its text alone, and comparing it to that text has to
/// be false. Without this the two would read as interchangeable at exactly the
/// place a caller is deciding whether anything was attached.
impl PartialEq<str> for TurnPrompt {
    fn eq(&self, other: &str) -> bool {
        self.attachments.is_empty() && self.text.as_str() == other
    }
}

impl PartialEq<&str> for TurnPrompt {
    fn eq(&self, other: &&str) -> bool {
        self == *other
    }
}

impl PartialEq<String> for TurnPrompt {
    fn eq(&self, other: &String) -> bool {
        self == other.as_str()
    }
}

/// Why host-originated text was not delivered at the steering boundary.
///
/// Both arms mean the same thing to a caller — the model will not see this
/// text — but they mean different things to the user, so a host that reports
/// what happened has to be able to tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerRefused {
    /// No run was mid-flight to carry it. Queueing it anyway would make it the
    /// next user prompt and start a turn nobody asked for.
    NoActiveRun,
    /// The text names a path that resolves outside the bound workspace.
    Rejected(MentionRejected),
}

#[derive(Debug, Clone)]
struct PendingInput {
    receipt: TurnInputReceipt,
    prompt: TurnPrompt,
}

#[derive(Debug, Default)]
struct State {
    active_run: Option<u64>,
    awaiting_prompt: bool,
    next_run: u64,
    next_id: u64,
    pending: VecDeque<PendingInput>,
    /// The directory `@` mentions are resolved against, bound by the agent
    /// that owns it. `None` until then, and a mention cannot be contained
    /// against nothing, so a mention-bearing submission is refused while it
    /// is unbound rather than admitted unchecked.
    working_dir: Option<PathBuf>,
}

/// Shared terminal input state consumed only by the existing turn driver and
/// agent graph.
#[derive(Clone, Debug)]
pub struct TurnSteering {
    enabled: bool,
    state: Arc<Mutex<State>>,
    notify: Arc<Notify>,
}

impl Default for TurnSteering {
    fn default() -> Self {
        Self {
            enabled: false,
            state: Arc::new(Mutex::new(State {
                next_run: 1,
                ..State::default()
            })),
            notify: Arc::new(Notify::new()),
        }
    }
}

impl LocalResource for TurnSteering {}

impl TurnSteering {
    /// Create steering state for an interactive front end.
    #[must_use]
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    /// Whether this front end routes prompts through the explicit queue.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Bind the directory this queue resolves `@` mentions against.
    ///
    /// The agent calls this from its own setup with the working directory it
    /// was configured with, which is deliberately the *same* value its tools
    /// are sandboxed to: threading a second copy in from the composition root
    /// would let the gate and the sandbox drift apart and disagree about what
    /// is inside the workspace. Binding is idempotent and takes effect for
    /// every handle sharing this queue, because the panel holds a clone.
    pub fn bind_working_dir(&self, working_dir: impl AsRef<Path>) {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        state.working_dir = Some(working_dir.as_ref().to_path_buf());
    }

    /// Resolve `text`'s mentions against the bound working directory.
    ///
    /// Every way into the queue goes through here, so there is exactly one
    /// answer per submission regardless of which mutator a host reached for.
    ///
    /// # Errors
    ///
    /// Returns the first mention that escapes the working directory, or — if
    /// no directory is bound yet — any mention at all: an unbound queue has
    /// nothing to resolve against, and admitting the prompt unchecked is the
    /// failure this gate exists to prevent. A prompt that mentions nothing is
    /// always accepted, so an unbound queue still carries ordinary text.
    fn contain(state: &State, text: String) -> Result<ContainedPrompt, MentionRejected> {
        match &state.working_dir {
            Some(working_dir) => ContainedPrompt::check(text, working_dir),
            None => ContainedPrompt::check_unbound(text),
        }
    }

    /// Mark the next run active. Called immediately before `process_turn_with`.
    pub fn activate(&self) -> u64 {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        let run = state.next_run.max(1);
        state.next_run = run.saturating_add(1);
        state.active_run = Some(run);
        state.awaiting_prompt = true;
        run
    }

    /// Wait for the queued prompt assigned to the active run.
    pub async fn receive_prompt(&self) -> TurnPrompt {
        loop {
            if let Some(text) = self.take_prompt() {
                return text;
            }
            self.notify.notified().await;
        }
    }

    fn take_prompt(&self) -> Option<TurnPrompt> {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        let run = state.active_run?;
        let index = state.pending.iter().position(|input| {
            input.receipt.mode == TurnInputMode::Queue && input.receipt.target_run == run
        })?;
        let input = state.pending.remove(index)?;
        state.awaiting_prompt = false;
        Some(input.prompt)
    }

    /// Drain steering text for the active run at a graph-owned safe boundary.
    pub fn take_steering(&self) -> Vec<TurnPrompt> {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        let Some(run) = state.active_run else {
            return Vec::new();
        };
        let mut drained = Vec::new();
        let mut retained = VecDeque::with_capacity(state.pending.len());
        while let Some(input) = state.pending.pop_front() {
            if input.receipt.mode == TurnInputMode::Steer && input.receipt.target_run == run {
                drained.push(input.prompt);
            } else {
                retained.push_back(input);
            }
        }
        state.pending = retained;
        drained
    }

    /// Deliver host-originated text at the mid-turn steering boundary, or not
    /// at all.
    ///
    /// Returns the receipt when the text was queued as [`TurnInputMode::Steer`]
    /// against a genuinely live run, and `None` — queueing **nothing** — when
    /// there is no such run. That asymmetry with
    /// [`submit`](TurnInputHandle::submit) is the point: a `Steer` submission
    /// with no active run degrades to `Queue`, which is to say it becomes the
    /// next user prompt and starts a turn. For a user's own text that is the
    /// right fallback; for a host reporting something that finished on its own
    /// it would spend the user's tokens on a model call they never asked for.
    /// A caller that gets `None` back has to report its news some other way.
    ///
    /// The text is host-authored rather than typed, but it is contained on the
    /// same rule as everything else the model is asked about: this queue holds
    /// [`ContainedPrompt`]s and there is no second way in. A refusal here is
    /// not a draft the user can fix, so the caller reports its news some other
    /// way — it does not become an unchecked prompt.
    ///
    /// # Errors
    ///
    /// [`SteerRefused::NoActiveRun`] when nothing is mid-flight, and
    /// [`SteerRefused::Rejected`] when the text names a path outside the bound
    /// workspace — host-authored or not, nothing reaches the model without
    /// passing the same containment check a user's prompt does.
    pub fn steer_mid_turn(&self, text: String) -> Result<TurnInputReceipt, SteerRefused> {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        let Some(run) = state.active_run else {
            return Err(SteerRefused::NoActiveRun);
        };
        if state.awaiting_prompt {
            return Err(SteerRefused::NoActiveRun);
        }
        let text = Self::contain(&state, text).map_err(SteerRefused::Rejected)?;
        let receipt = TurnInputReceipt {
            id: state.next_id,
            target_run: run,
            mode: TurnInputMode::Steer,
        };
        state.next_id = state.next_id.wrapping_add(1);
        state.pending.push_back(PendingInput {
            receipt: receipt.clone(),
            prompt: TurnPrompt::new(text),
        });
        drop(state);
        self.notify.notify_waiters();
        Ok(receipt)
    }

    /// End the active run without losing steering text that never reached a
    /// safe boundary. Such text becomes ordered follow-up runs.
    pub fn finish(&self) {
        let mut state = self.state.lock().expect("turn steering mutex poisoned");
        let Some(run) = state.active_run.take() else {
            return;
        };
        state.awaiting_prompt = false;
        let mut target = state
            .pending
            .iter()
            .map(|input| input.receipt.target_run)
            .max()
            .unwrap_or(state.next_run.saturating_sub(1))
            .max(run);
        for input in &mut state.pending {
            if input.receipt.mode == TurnInputMode::Steer && input.receipt.target_run == run {
                target = target.saturating_add(1);
                input.receipt.mode = TurnInputMode::Queue;
                input.receipt.target_run = target;
            }
        }
        state.next_run = state.next_run.max(run.saturating_add(1));
        self.notify.notify_waiters();
    }
}

impl TurnInputHandle for TurnSteering {
    fn submit(&self, text: String, requested: TurnInputMode) -> Result<TurnInputReceipt, String> {
        self.submit_with_attachments(text, requested, Vec::new())
    }

    fn submit_with_attachments(
        &self,
        text: String,
        requested: TurnInputMode,
        attachments: Vec<String>,
    ) -> Result<TurnInputReceipt, String> {
        if text.trim().is_empty() {
            return Err("empty turn input was not queued".into());
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| "turn input queue unavailable")?;
        // Refuse before anything is queued and before a receipt exists, so
        // the panel keeps the draft and its chips and the user can correct
        // the mention in place. Nothing is consumed by a refusal.
        let text = Self::contain(&state, text).map_err(|rejected| rejected.notice())?;
        let mode = match (requested, state.active_run, state.awaiting_prompt) {
            (TurnInputMode::Steer, Some(_), false) => TurnInputMode::Steer,
            _ => TurnInputMode::Queue,
        };
        let target_run = match mode {
            TurnInputMode::Steer => state.active_run.expect("steering requires active run"),
            TurnInputMode::Queue => state
                .pending
                .iter()
                .filter(|input| input.receipt.mode == TurnInputMode::Queue)
                .map(|input| input.receipt.target_run)
                .max()
                .unwrap_or_else(|| match (state.active_run, state.awaiting_prompt) {
                    (Some(run), true) => run,
                    (Some(run), false) => run.saturating_add(1),
                    (None, _) => state.next_run,
                }),
        };
        let target_run = if mode == TurnInputMode::Queue
            && state.pending.iter().any(|input| {
                input.receipt.mode == TurnInputMode::Queue && input.receipt.target_run == target_run
            }) {
            target_run.saturating_add(1)
        } else {
            target_run
        };
        let receipt = TurnInputReceipt {
            id: state.next_id,
            target_run,
            mode,
        };
        state.next_id = state.next_id.wrapping_add(1);
        state.pending.push_back(PendingInput {
            receipt: receipt.clone(),
            prompt: TurnPrompt { text, attachments },
        });
        drop(state);
        self.notify.notify_waiters();
        Ok(receipt)
    }

    fn replace(&self, id: u64, text: String) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "turn input queue unavailable")?;
        // Checked first, and on the same rule as a fresh submission: a
        // rewrite is a submission of new text, and a contained prompt must
        // not be able to become an escaping one in place. The entry is left
        // exactly as it was when the rewrite is refused.
        let text = Self::contain(&state, text).map_err(|rejected| rejected.notice())?;
        let input = state
            .pending
            .iter_mut()
            .find(|input| input.receipt.id == id)
            .ok_or_else(|| "message was already consumed".to_string())?;
        // Only the text is replaced. The composer recalled the text into the
        // draft, and the chips it recalled alongside are the same ones; a
        // genuine change of attachment goes through remove-and-resubmit.
        input.prompt.text = text;
        Ok(())
    }

    fn remove(&self, id: u64) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "turn input queue unavailable")?;
        let index = state
            .pending
            .iter()
            .position(|input| input.receipt.id == id)
            .ok_or_else(|| "message was already consumed".to_string())?;
        state.pending.remove(index);
        Ok(())
    }
}
