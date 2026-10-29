//! Repository-scoped terminal session commands.
//!
//! The commands schedule a host action; they never resume or save while the
//! agent graph holds the session context lock. [`CommandDispatch`](super::CommandDispatch)
//! ends the idle input turn, then the composition root performs the transition.
//!
//! # Choosing a session
//!
//! `/sessions` is the picker. It numbers the repository's saved sessions,
//! newest first, and `/resume <number>` chooses one of those rows, so coming
//! back to yesterday's work is recognising it rather than knowing its
//! identifier. A number is only ever a row of the list the user was just
//! shown: it resolves against a freshly listed store at the moment of the
//! choice, so nothing here caches what a session is, and `/resume <id>` stays
//! exactly as it was for anyone who does know the identifier.
//!
//! The choice is not a second way to move the active session. It ends in the
//! same [`schedule`](SessionNavigation::schedule) slot `/new` and `/resume <id>`
//! have always used, which the composition root drains at the turn boundary.
//!
//! # What a row can say
//!
//! Recognition needs more than an identifier. Given a
//! [`SessionStore`](polaris::sessions::SessionStore) to read — supplied by the
//! composition root through [`with_store`](SessionNavigation::with_store) —
//! each row also carries the session's turn count, when it started, and the
//! first prompt it was opened with, all read straight out of the stored
//! [`SessionData`] with no second copy kept anywhere. Without a reader the
//! picker degrades to identifiers and still numbers and still chooses.
//!
//! *When it was last active* is deliberately absent: the stored session
//! records `created_at` and nothing else about time; the stored format has no
//! field for it.

use std::sync::{Arc, Mutex, RwLock};

use polaris::models::llm::{Message, UserBlock};
use polaris::plugins::Storable;
use polaris::sessions::{SessionData, SessionId, SessionStore, SessionsAPI, TurnNumber};
use polaris_tui::commands::{CommandInvocation, CommandSpec};

use crate::agents::ContextManager;

use super::{CommandFuture, CommandHandler, CommandOutcome, CommandRegistry};

/// Most rows the picker numbers. Enough to reach back through a week of work;
/// few enough that the list stays scannable in the transcript. Older sessions
/// are counted, named as unreachable by number, and still resumable by id.
const PICKER_ROWS: usize = 20;

/// Longest opening prompt a row shows before it is elided.
const OPENING_CHARS: usize = 72;

/// A session transition requested through the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// Create a fresh session, optionally using the requested identity.
    New(SessionId),
    /// Resume an existing persisted session.
    Resume(SessionId),
}

/// One saved session as a picker row describes it.
///
/// Every field beyond the identity is optional because every one of them is a
/// read of the store that can be absent: no store reader was supplied, the
/// session was written by something that persisted no conversation, or the
/// record would not parse. A row says what it knows and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionChoice {
    /// The stored identity, which `/resume <id>` still accepts.
    id: SessionId,
    /// Turns the session had when it was last saved.
    turns: Option<TurnNumber>,
    /// ISO 8601 UTC timestamp of when the session was created.
    started: Option<String>,
    /// The first prompt the session was opened with, on one line.
    opening: Option<String>,
}

/// The rows the picker shows, and how many it left out.
#[derive(Debug, Clone)]
struct Picker {
    shown: Vec<SessionChoice>,
    hidden: usize,
}

/// Shared state behind `/sessions`, `/new`, and `/resume`.
#[derive(Clone)]
pub struct SessionNavigation {
    sessions: SessionsAPI,
    store: Option<Arc<dyn SessionStore>>,
    current: Arc<RwLock<SessionId>>,
    pending: Arc<Mutex<Option<SessionAction>>>,
}

impl SessionNavigation {
    /// Construct navigation for the initial session.
    #[must_use]
    pub fn new(sessions: SessionsAPI, current: SessionId) -> Self {
        Self {
            sessions,
            store: None,
            current: Arc::new(RwLock::new(current)),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    /// Read saved sessions through `store` so the picker can describe them.
    ///
    /// Pass the same store the session plugin was built with. This is a read
    /// path only — the picker loads a record to render a row and keeps
    /// nothing; every listing reads the store again.
    #[must_use]
    pub fn with_store(mut self, store: Arc<dyn SessionStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Register the repository-scoped session commands.
    pub fn register(&self, registry: &CommandRegistry) -> Result<(), super::CommandError> {
        let list = self.clone();
        registry.register(
            CommandSpec::new(
                "sessions",
                "Choose from the sessions saved for this repository",
            ),
            handler(move |_| {
                let list = list.clone();
                async move { list.list().await }
            }),
        )?;

        let new = self.clone();
        registry.register(
            CommandSpec::new("new", "Start a new session").with_usage("[id]"),
            handler(move |invocation| {
                let new = new.clone();
                async move {
                    let id = if invocation.args.trim().is_empty() {
                        SessionId::new()
                    } else {
                        SessionId::from_string(invocation.args.trim())
                    };
                    new.schedule(SessionAction::New(id))
                }
            }),
        )?;

        let resume = self.clone();
        registry.register(
            CommandSpec::new("resume", "Resume a saved session").with_usage("[number|id]"),
            handler(move |invocation| {
                let resume = resume.clone();
                async move { resume.resume(invocation.args.trim()).await }
            }),
        )
    }

    /// `/sessions`: the picker.
    async fn list(&self) -> CommandOutcome {
        match self.picker().await {
            Ok(picker) => CommandOutcome::Reply(self.render(&picker)),
            Err(outcome) => outcome,
        }
    }

    /// `/resume [number|id]`: choose a row, or name a session outright.
    async fn resume(&self, value: &str) -> CommandOutcome {
        if value.is_empty() {
            return self.list().await;
        }
        let Ok(position) = value.parse::<usize>() else {
            return self.schedule(SessionAction::Resume(SessionId::from_string(value)));
        };
        let picker = match self.picker().await {
            Ok(picker) => picker,
            Err(outcome) => return outcome,
        };
        // A session may be *named* `2`. The identifier wins, so knowing one
        // never means counting rows to use it.
        if picker.shown.iter().any(|row| row.id.as_str() == value) {
            return self.schedule(SessionAction::Resume(SessionId::from_string(value)));
        }
        if picker.shown.is_empty() {
            return CommandOutcome::Error(EMPTY.into());
        }
        match position
            .checked_sub(1)
            .and_then(|index| picker.shown.get(index))
        {
            Some(choice) => self.schedule(SessionAction::Resume(choice.id.clone())),
            None => CommandOutcome::Error(format!(
                "`{value}` is not on the list — choose 1–{}, or pass a session id. Run `/sessions` to see them.",
                picker.shown.len()
            )),
        }
    }

    /// The rows, newest first, read fresh from the store on every call.
    ///
    /// The error arm is the outcome to return, so the one sentence explaining
    /// an unreadable store is written once.
    async fn picker(&self) -> Result<Picker, CommandOutcome> {
        let ids = self.sessions.list_sessions().await.map_err(|err| {
            CommandOutcome::Error(format!(
                "could not list repository sessions: {err}. Fix access to the session store and retry `/sessions`."
            ))
        })?;
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let described = match &self.store {
                Some(store) => store.load(&id).await.ok().flatten(),
                None => None,
            };
            rows.push(describe(id, described.as_ref()));
        }
        rows.sort_by(|left, right| match (&left.started, &right.started) {
            (Some(left_at), Some(right_at)) => right_at
                .cmp(left_at)
                .then_with(|| left.id.as_str().cmp(right.id.as_str())),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => left.id.as_str().cmp(right.id.as_str()),
        });
        let hidden = rows.len().saturating_sub(PICKER_ROWS);
        rows.truncate(PICKER_ROWS);
        Ok(Picker {
            shown: rows,
            hidden,
        })
    }

    fn render(&self, picker: &Picker) -> String {
        let current = self.current();
        let mut text = format!(
            "Sessions for this repository (current `{}`):\n",
            current.as_str()
        );
        if picker.shown.is_empty() {
            text.push('\n');
            text.push_str(EMPTY);
            return text;
        }
        for (index, row) in picker.shown.iter().enumerate() {
            text.push('\n');
            text.push_str(&row.render(index + 1, &current));
        }
        text.push_str("\n\nResume one with `/resume <number>`, or `/new` to start another.");
        if picker.hidden > 0 {
            text.push_str(&format!(
                "\n{} older sessions are not numbered; reach one with `/resume <id>`.",
                picker.hidden
            ));
        }
        text
    }

    fn schedule(&self, action: SessionAction) -> CommandOutcome {
        let target = match &action {
            SessionAction::New(id) | SessionAction::Resume(id) => id.as_str().to_owned(),
        };
        if target == self.current().as_str() {
            return CommandOutcome::Error(format!("session `{target}` is already active"));
        }
        let mut pending = self.pending.lock().unwrap_or_else(|err| err.into_inner());
        if pending.is_some() {
            return CommandOutcome::Error("a session switch is already pending".into());
        }
        *pending = Some(action);
        CommandOutcome::Control(format!(
            "Switching to session `{target}` after the current input boundary…"
        ))
    }

    /// Take the pending action after the graph releases the session context.
    pub fn take_pending(&self) -> Option<SessionAction> {
        self.pending
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .take()
    }

    /// Update the identity after a successful transition.
    pub fn set_current(&self, id: SessionId) {
        *self.current.write().unwrap_or_else(|err| err.into_inner()) = id;
    }

    /// The current session identity.
    #[must_use]
    pub fn current(&self) -> SessionId {
        self.current
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }
}

/// What both entry points say when the repository has saved nothing yet.
const EMPTY: &str = "No sessions are saved for this repository yet. Use `/new` to start one.";

impl SessionChoice {
    /// One numbered row: the identity, then whatever is known about it.
    fn render(&self, position: usize, current: &SessionId) -> String {
        let marker = if self.id == *current {
            " (current)"
        } else {
            ""
        };
        let mut row = format!("{position}. `{}`{marker}", self.id.as_str());
        if let Some(turns) = self.turns {
            row.push_str(&format!(
                " — {turns} turn{}",
                if turns == 1 { "" } else { "s" }
            ));
        }
        if let Some(started) = self.started.as_deref().map(readable_timestamp) {
            row.push_str(&format!(", started {started}"));
        }
        if let Some(opening) = &self.opening {
            row.push_str(&format!(" — \"{opening}\""));
        }
        row
    }
}

/// Read a row out of a stored session record.
fn describe(id: SessionId, data: Option<&SessionData>) -> SessionChoice {
    let Some(data) = data else {
        return SessionChoice {
            id,
            turns: None,
            started: None,
            opening: None,
        };
    };
    SessionChoice {
        id,
        turns: Some(data.turn_number),
        started: Some(data.created_at.clone()),
        opening: opening_prompt(data),
    }
}

/// The first thing the user asked this session for, on one short line.
///
/// Read from the agent's own persisted conversation, which is the only place
/// a stored session keeps it. A record without one — a session saved before
/// its first prompt, or by a different agent — simply has no opening.
fn opening_prompt(data: &SessionData) -> Option<String> {
    let entry = data
        .resources
        .iter()
        .find(|entry| entry.storage_key == ContextManager::storage_key())?;
    let context: ContextManager = serde_json::from_value(entry.data.clone()).ok()?;
    let text = context.messages.iter().find_map(|message| match message {
        Message::User { content } => content.iter().find_map(|block| match block {
            UserBlock::Text(text) => Some(text.text.as_str()),
            // An opening the user cannot read back as one line is no opening.
            _ => None,
        }),
        Message::Assistant { .. } => None,
    })?;
    let mut collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return None;
    }
    if collapsed.chars().count() > OPENING_CHARS {
        let cut = collapsed
            .char_indices()
            .nth(OPENING_CHARS)
            .map_or(collapsed.len(), |(offset, _)| offset);
        collapsed.truncate(cut);
        collapsed.push('…');
    }
    Some(collapsed)
}

/// `2026-09-20T09:30:00Z` → `2026-09-20 09:30 UTC`. Anything that is not an
/// ISO 8601 UTC instant is shown exactly as it was stored rather than guessed
/// at.
fn readable_timestamp(stored: &str) -> String {
    let bytes = stored.as_bytes();
    if bytes.len() >= 16 && bytes[10] == b'T' && stored.is_char_boundary(16) {
        format!("{} {} UTC", &stored[..10], &stored[11..16])
    } else {
        stored.to_owned()
    }
}

fn handler<F, Fut>(function: F) -> CommandHandler
where
    F: Fn(CommandInvocation) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = CommandOutcome> + Send + 'static,
{
    Arc::new(move |ctx| {
        let invocation = CommandInvocation::new("", ctx.args);
        Box::pin(function(invocation)) as CommandFuture<'_>
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use polaris::sessions::store::memory::InMemoryStore;

    use super::*;

    #[tokio::test]
    async fn session_commands_schedule_one_boundary_switch_at_a_time() {
        let sessions = SessionsAPI::new(Arc::new(InMemoryStore::new()));
        let navigation = SessionNavigation::new(sessions, SessionId::from_string("current"));
        let registry = CommandRegistry::with_builtins();
        navigation.register(&registry).unwrap();

        assert!(registry.contains("sessions"));
        assert!(registry.contains("new"));
        assert!(registry.contains("resume"));
        assert!(matches!(
            registry
                .dispatch(&CommandInvocation::new("resume", "next"))
                .await,
            CommandOutcome::Control(_)
        ));
        assert!(matches!(
            registry
                .dispatch(&CommandInvocation::new("new", "third"))
                .await,
            CommandOutcome::Error(message) if message.contains("already pending")
        ));
        assert_eq!(
            navigation.take_pending(),
            Some(SessionAction::Resume(SessionId::from_string("next")))
        );
    }

    #[test]
    fn a_row_says_only_what_the_record_knows() {
        let current = SessionId::from_string("default");
        let bare = SessionChoice {
            id: SessionId::from_string("default"),
            turns: None,
            started: None,
            opening: None,
        };
        assert_eq!(bare.render(3, &current), "3. `default` (current)");

        let full = SessionChoice {
            id: SessionId::from_string("abc"),
            turns: Some(1),
            started: Some("2026-09-20T09:30:00Z".into()),
            opening: Some("fix the retry test".into()),
        };
        assert_eq!(
            full.render(1, &current),
            "1. `abc` — 1 turn, started 2026-09-20 09:30 UTC — \"fix the retry test\""
        );
    }

    #[test]
    fn a_timestamp_that_is_not_an_instant_is_shown_as_stored() {
        assert_eq!(
            readable_timestamp("2026-09-20T09:30:00Z"),
            "2026-09-20 09:30 UTC"
        );
        assert_eq!(readable_timestamp("yesterday"), "yesterday");
        assert_eq!(readable_timestamp(""), "");
        // A multibyte glyph where the minute field would be never panics.
        assert_eq!(
            readable_timestamp("2026-09-20T09:3日0Z"),
            "2026-09-20T09:3日0Z"
        );
    }
}
