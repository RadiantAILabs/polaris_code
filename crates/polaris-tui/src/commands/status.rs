//! The host → panel *status report*: what the session behind the transcript is,
//! how full its context window is, what it has cost, and whether the host can
//! still process work.
//!
//! This rides the same `io_type = "status"` message the command advertisement
//! does (see the [module docs](super)), under its own metadata key, so it needs
//! no new transport and no new framework type. The panel renders it on the meta
//! row above the composer.
//!
//! # The TUI owns none of it
//!
//! Every field is the **host's** assertion, carried opaquely:
//!
//! - [`model`](StatusReport::model), [`workspace`](StatusReport::workspace) and
//!   [`session`](StatusReport::session) are labels. The panel never parses a
//!   model id, a path or a session key, and ships no list of any of them.
//! - [`permissions`](StatusReport::permissions) is the host's own words for its
//!   current approval posture. There is no permission vocabulary here, and no
//!   mode set: a host that gains one describes it in this string, and the panel
//!   is unchanged.
//! - [`context`](StatusReport::context) is a token count against an optional
//!   budget. What a token is, what the budget is and when to compact are the
//!   host's policy; the panel only draws the ratio.
//! - [`cost`](StatusReport::cost) is a number the host priced, or the host's
//!   statement that it *cannot* price this model. The panel shows the second
//!   as "n/a" rather than as a zero — an unpriced session is not a free one.
//! - [`tasks_running`](StatusReport::tasks_running) is how many host-owned
//!   tasks are running outside the current turn. A count and nothing else:
//!   the panel holds no task list, no identity type and no lifecycle for one,
//!   and cannot tell a background process from any other kind of host work.
//!
//! # Reports are partial
//!
//! A report updates only the fields it carries. An absent field — `None`, or
//! [`Availability::Unstated`] — leaves the panel's cached value alone, so a host
//! can send the identity once and a cheap context/cost refresh every turn
//! without the two fighting. [`StatusReport::merge`] is that rule, and it is the
//! only way the panel folds a report in.
//!
//! ```
//! use polaris_tui::commands::{ContextWindow, StatusReport, parse_status, report_status};
//!
//! let full = StatusReport::default()
//!     .with_model("anthropic/claude-sonnet-4-6")
//!     .with_context(ContextWindow::new(48_000).with_limit(120_000));
//! let message = report_status(&full);
//! assert_eq!(parse_status(&message).unwrap(), full);
//!
//! // A later refresh carries only the window; the model survives it.
//! let mut cached = full.clone();
//! cached.merge(StatusReport::default().with_context(ContextWindow::new(61_000)));
//! assert_eq!(cached.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
//! assert_eq!(cached.context.unwrap().tokens, 61_000);
//! ```

use polaris::plugins::{IO_TYPE, IOMessage};

/// Metadata key on a `status` message carrying a [`StatusReport`] as a JSON
/// object. Its presence is what makes the message a report.
pub const META_STATUS_REPORT: &str = "status_report";
/// `io_type` value a report rides on — the panel's existing status vocabulary.
const TYPE_STATUS: &str = "status";

/// Whether the host can process submitted work.
///
/// Three states rather than a `bool`, because "the host has said nothing" is
/// not the same as "the host says it is fine". Only the host moves this: the
/// panel never infers unavailability from a timer, a quiet channel, or a key
/// press.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Availability {
    /// The host has not spoken. Merging this leaves the cached value untouched.
    #[default]
    Unstated,
    /// The host reports it can take work. Clears a previous
    /// [`Unavailable`](Self::Unavailable).
    Available,
    /// The host reports it cannot process work, and why.
    ///
    /// The reason is shown to the user verbatim, so it should say what happened
    /// and what to do about it — not just that something went wrong.
    Unavailable(String),
}

impl Availability {
    /// The reason work cannot be processed, if the host reported one.
    #[must_use]
    pub fn unavailable_reason(&self) -> Option<&str> {
        match self {
            Self::Unavailable(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// What the session has cost so far, as the host prices it.
///
/// Three states, for the same reason [`Availability`] has three: "the host has
/// not said" and "the host cannot say" are different facts. A host with no
/// price table for its model reports [`Unpriced`](Self::Unpriced), and the
/// panel prints `n/a` — never `$0.00`, which would read as free.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Cost {
    /// The host has not spoken. Merging this leaves the cached value untouched.
    #[default]
    Unstated,
    /// The host tried to price the session and could not.
    Unpriced,
    /// Estimated spend so far, in USD.
    Usd(f64),
}

impl Cost {
    /// The priced amount, if the host priced it.
    #[must_use]
    pub fn usd(self) -> Option<f64> {
        match self {
            Self::Usd(amount) => Some(amount),
            _ => None,
        }
    }
}

/// How full the window the host will send on its next model call is.
///
/// This is deliberately *not* the cumulative token tally the panel already
/// keeps from `io_type = "tokens"` deltas. That tally is everything billed so
/// far this session and only ever grows; this is the size of one prompt, and it
/// falls when the host compacts or slides its window. Conflating them makes a
/// long session look like it is about to overflow when it is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextWindow {
    /// Tokens the next projected window occupies.
    pub tokens: u64,
    /// The budget the host enforces, when it enforces one. `None` means there
    /// is no budget to show a ratio against — not that the budget is zero.
    pub limit: Option<u64>,
    /// Whether [`tokens`](Self::tokens) is an estimate the host has **not**
    /// calibrated against a real provider reading. Rendered with a `~` so an
    /// uncalibrated guess never reads as a measurement.
    pub estimated: bool,
    /// Whether the host is currently compacting to stay inside the budget.
    pub compacting: bool,
}

impl ContextWindow {
    /// A window of `tokens`, with no budget and no compaction claimed.
    #[must_use]
    pub fn new(tokens: u64) -> Self {
        Self {
            tokens,
            ..Self::default()
        }
    }

    /// Declare the budget the host enforces.
    #[must_use]
    pub fn with_limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Mark the count an uncalibrated estimate.
    #[must_use]
    pub fn estimated(mut self, estimated: bool) -> Self {
        self.estimated = estimated;
        self
    }

    /// Mark the host as compacting to fit the budget.
    #[must_use]
    pub fn compacting(mut self, compacting: bool) -> Self {
        self.compacting = compacting;
        self
    }

    /// Percent of the budget in use, rounded down. `None` without a budget, and
    /// `None` for a zero budget rather than a division by zero.
    #[must_use]
    pub fn percent(&self) -> Option<u64> {
        match self.limit {
            Some(limit) if limit > 0 => Some(self.tokens.saturating_mul(100) / limit),
            _ => None,
        }
    }
}

/// What the host reports about the session behind the transcript.
///
/// Build one with the `with_*` methods, send it with [`report_status`], and read
/// it back with [`parse_status`]. Every field is optional and independently
/// updatable; see the [module docs](self) for the merge rule.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StatusReport {
    /// The model in use, as the host names it.
    pub model: Option<String>,
    /// Where the work happens — a repository or directory label.
    pub workspace: Option<String>,
    /// Which session this is, as the host names it.
    pub session: Option<String>,
    /// The host's own description of its current approval posture.
    pub permissions: Option<String>,
    /// How full the next projected prompt window is.
    pub context: Option<ContextWindow>,
    /// What the session has cost so far.
    pub cost: Cost,
    /// Whether the host can process work.
    pub availability: Availability,
    /// How many host-owned tasks are running outside the current turn.
    ///
    /// Generic on purpose: this is a count, not a task list. The panel never
    /// learns what a task *is* — whether it is a process, a job or a request —
    /// and holds no registry, no identity type and no state machine for one.
    pub tasks_running: Option<u64>,
}

impl StatusReport {
    /// Name the model.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Name the workspace.
    #[must_use]
    pub fn with_workspace(mut self, workspace: impl Into<String>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    /// Name the session.
    #[must_use]
    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(session.into());
        self
    }

    /// Describe the approval posture.
    #[must_use]
    pub fn with_permissions(mut self, permissions: impl Into<String>) -> Self {
        self.permissions = Some(permissions.into());
        self
    }

    /// Report the projected context window.
    #[must_use]
    pub fn with_context(mut self, context: ContextWindow) -> Self {
        self.context = Some(context);
        self
    }

    /// Report the estimated spend in USD.
    #[must_use]
    pub fn with_cost_usd(mut self, cost_usd: f64) -> Self {
        self.cost = Cost::Usd(cost_usd);
        self
    }

    /// Report that the session cannot be priced.
    #[must_use]
    pub fn unpriced(mut self) -> Self {
        self.cost = Cost::Unpriced;
        self
    }

    /// Report availability.
    #[must_use]
    pub fn with_availability(mut self, availability: Availability) -> Self {
        self.availability = availability;
        self
    }

    /// Report how many host-owned tasks are running outside the current turn.
    ///
    /// Rendered as one more dim fact on the meta row, so work the user has
    /// pushed off-screen stays discoverable. Reporting `0` is how a host
    /// retracts a previous count: it merges like any other asserted value and
    /// renders nothing, because there is nothing left to discover.
    #[must_use]
    pub fn with_tasks_running(mut self, running: u64) -> Self {
        self.tasks_running = Some(running);
        self
    }

    /// Whether this report asserts nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Fold `update` into `self`, keeping every value `update` does not assert.
    ///
    /// This is what makes reports partial: a host may re-send only the fields
    /// that changed, and nothing it left out is forgotten. In particular an
    /// [`Availability::Unstated`] update never resurrects a host that reported
    /// itself unavailable.
    pub fn merge(&mut self, update: Self) {
        let Self {
            model,
            workspace,
            session,
            permissions,
            context,
            cost,
            availability,
            tasks_running,
        } = update;
        if model.is_some() {
            self.model = model;
        }
        if workspace.is_some() {
            self.workspace = workspace;
        }
        if session.is_some() {
            self.session = session;
        }
        if permissions.is_some() {
            self.permissions = permissions;
        }
        if context.is_some() {
            self.context = context;
        }
        if cost != Cost::Unstated {
            self.cost = cost;
        }
        if availability != Availability::Unstated {
            self.availability = availability;
        }
        if tasks_running.is_some() {
            self.tasks_running = tasks_running;
        }
    }
}

/// Build the `status` message a host sends to report `report`.
///
/// The text is empty, so a panel that treats every `status` as an activity
/// update sees "idle" — but the conversation panel recognises
/// [`META_STATUS_REPORT`] and consumes the message as a report only, leaving the
/// activity line and the spinner alone.
#[must_use]
pub fn report_status(report: &StatusReport) -> IOMessage {
    let mut object = serde_json::Map::new();
    let mut put = |key: &str, value: Option<serde_json::Value>| {
        if let Some(value) = value {
            object.insert(key.to_string(), value);
        }
    };
    put("model", report.model.clone().map(Into::into));
    put("workspace", report.workspace.clone().map(Into::into));
    put("session", report.session.clone().map(Into::into));
    put("permissions", report.permissions.clone().map(Into::into));
    put(
        "context",
        report.context.map(|window| {
            let mut context = serde_json::Map::new();
            context.insert("tokens".into(), window.tokens.into());
            if let Some(limit) = window.limit {
                context.insert("limit".into(), limit.into());
            }
            if window.estimated {
                context.insert("estimated".into(), true.into());
            }
            if window.compacting {
                context.insert("compacting".into(), true.into());
            }
            serde_json::Value::Object(context)
        }),
    );
    put(
        "cost_usd",
        match report.cost {
            Cost::Unstated => None,
            // `null` is the host saying "no price", distinct from saying nothing.
            Cost::Unpriced => Some(serde_json::Value::Null),
            Cost::Usd(amount) => Some(
                serde_json::Number::from_f64(amount)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number),
            ),
        },
    );
    put("tasks_running", report.tasks_running.map(Into::into));
    put(
        "availability",
        match &report.availability {
            Availability::Unstated => None,
            Availability::Available => Some("available".into()),
            Availability::Unavailable(reason) => Some(serde_json::json!({
                "unavailable": reason,
            })),
        },
    );
    IOMessage::system_text("")
        .with_metadata(IO_TYPE, TYPE_STATUS)
        .with_metadata(
            META_STATUS_REPORT,
            serde_json::Value::Object(object).to_string(),
        )
}

/// The `status` message reporting the host cannot process work, and why.
///
/// A one-call shorthand for the state that matters most: the composer must stop
/// looking ready the moment the host knows it is not. Pair it with
/// [`Availability::Available`] (or a plain [`report_status`] carrying it) once
/// the host can take work again.
#[must_use]
pub fn report_unavailable(reason: impl Into<String>) -> IOMessage {
    report_status(
        &StatusReport::default().with_availability(Availability::Unavailable(reason.into())),
    )
}

/// Read a status report, if `message` is one.
///
/// Returns `None` for a message without the [`META_STATUS_REPORT`] key. A
/// payload that does not parse, or a field of the wrong shape, yields the
/// *empty* report rather than a guess: a host that sends garbage has reported
/// nothing, and merging nothing changes nothing.
#[must_use]
pub fn parse_status(message: &IOMessage) -> Option<StatusReport> {
    let raw = message.metadata.get(META_STATUS_REPORT)?;
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(raw)
    else {
        return Some(StatusReport::default());
    };
    let text = |key: &str| {
        object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let context = object.get("context").and_then(|value| {
        let context = value.as_object()?;
        Some(ContextWindow {
            tokens: context.get("tokens").and_then(serde_json::Value::as_u64)?,
            limit: context.get("limit").and_then(serde_json::Value::as_u64),
            estimated: context
                .get("estimated")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            compacting: context
                .get("compacting")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    });
    let availability = match object.get("availability") {
        Some(serde_json::Value::String(state)) if state == "available" => Availability::Available,
        Some(serde_json::Value::Object(state)) => state
            .get("unavailable")
            .and_then(serde_json::Value::as_str)
            .map_or(Availability::Unstated, |reason| {
                Availability::Unavailable(reason.to_string())
            }),
        _ => Availability::Unstated,
    };
    Some(StatusReport {
        model: text("model"),
        workspace: text("workspace"),
        session: text("session"),
        permissions: text("permissions"),
        context,
        cost: match object.get("cost_usd") {
            // An explicit `null` is the host saying it has no price.
            Some(serde_json::Value::Null) => Cost::Unpriced,
            Some(serde_json::Value::Number(amount)) => {
                amount.as_f64().map_or(Cost::Unpriced, Cost::Usd)
            }
            // Absent, or not a number at all: the host has said nothing usable.
            _ => Cost::Unstated,
        },
        availability,
        tasks_running: object
            .get("tasks_running")
            .and_then(serde_json::Value::as_u64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> StatusReport {
        StatusReport::default()
            .with_model("anthropic/claude-sonnet-4-6")
            .with_workspace("polaris_code")
            .with_session("default · turn 3")
            .with_permissions("2 tools ask first")
            .with_context(
                ContextWindow::new(48_100)
                    .with_limit(120_000)
                    .estimated(true)
                    .compacting(true),
            )
            .with_cost_usd(0.0412)
            .with_availability(Availability::Available)
    }

    #[test]
    fn a_full_report_round_trips() {
        let report = full();
        let message = report_status(&report);
        assert_eq!(message.metadata.get(IO_TYPE).unwrap(), TYPE_STATUS);
        assert_eq!(parse_status(&message), Some(report));
    }

    #[test]
    fn an_empty_report_round_trips_to_empty() {
        let message = report_status(&StatusReport::default());
        let parsed = parse_status(&message).expect("the key is present");
        assert!(parsed.is_empty(), "{parsed:?}");
    }

    #[test]
    fn a_message_without_the_key_is_not_a_report() {
        assert!(parse_status(&IOMessage::system_text("hello")).is_none());
        // An advertisement is a `status` message too, and must not be mistaken
        // for a report — otherwise every advertisement would blank the row.
        let advertisement = super::super::advertise_commands(&[]);
        assert!(parse_status(&advertisement).is_none());
    }

    #[test]
    fn garbage_reports_nothing_rather_than_guessing() {
        let message = IOMessage::system_text("")
            .with_metadata(IO_TYPE, TYPE_STATUS)
            .with_metadata(META_STATUS_REPORT, "not json at all");
        assert_eq!(parse_status(&message), Some(StatusReport::default()));

        // A well-formed object with ill-typed fields keeps what it can and
        // drops the rest; nothing becomes a fabricated default.
        let message = IOMessage::system_text("")
            .with_metadata(IO_TYPE, TYPE_STATUS)
            .with_metadata(
                META_STATUS_REPORT,
                r#"{"model":7,"cost_usd":"free","context":{"limit":10},"availability":"maybe"}"#,
            );
        let parsed = parse_status(&message).unwrap();
        assert_eq!(parsed.model, None);
        // An ill-typed price is dropped like any other ill-typed field; only
        // an explicit `null` means "unpriced" (see the test below).
        assert_eq!(parsed.cost, Cost::Unstated);
        // `tokens` is the one required field of a window: without it there is
        // no window, only a budget nobody is spending against.
        assert_eq!(parsed.context, None);
        assert_eq!(parsed.availability, Availability::Unstated);
    }

    #[test]
    fn merge_keeps_everything_the_update_does_not_assert() {
        let mut cached = full();
        cached.merge(StatusReport::default().with_context(ContextWindow::new(9)));
        assert_eq!(cached.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
        assert_eq!(cached.permissions.as_deref(), Some("2 tools ask first"));
        assert_eq!(cached.cost, Cost::Usd(0.0412));
        assert_eq!(cached.context, Some(ContextWindow::new(9)));
        assert_eq!(cached.availability, Availability::Available);
    }

    #[test]
    fn an_unstated_update_cannot_resurrect_an_unavailable_host() {
        let mut cached = StatusReport::default()
            .with_availability(Availability::Unavailable("the driver stopped".into()));
        cached.merge(StatusReport::default().with_cost_usd(1.0));
        assert_eq!(
            cached.availability.unavailable_reason(),
            Some("the driver stopped")
        );
        // Only an explicit `Available` clears it.
        cached.merge(StatusReport::default().with_availability(Availability::Available));
        assert_eq!(cached.availability, Availability::Available);
    }

    #[test]
    fn unavailable_round_trips_with_its_reason() {
        let message = report_unavailable("the turn driver stopped after a failed turn");
        let parsed = parse_status(&message).unwrap();
        assert_eq!(
            parsed.availability.unavailable_reason(),
            Some("the turn driver stopped after a failed turn")
        );
        // It asserts nothing else, so merging it disturbs no other field.
        assert_eq!(parsed.model, None);
        assert_eq!(parsed.context, None);
    }

    #[test]
    fn percent_needs_a_nonzero_budget() {
        assert_eq!(ContextWindow::new(50).percent(), None);
        assert_eq!(ContextWindow::new(50).with_limit(0).percent(), None);
        assert_eq!(ContextWindow::new(50).with_limit(200).percent(), Some(25));
        // Over budget reports over 100 rather than clamping to a comfortable lie.
        assert_eq!(ContextWindow::new(300).with_limit(200).percent(), Some(150));
    }

    #[test]
    fn a_non_finite_cost_is_reported_as_unpriced() {
        // `serde_json` cannot represent NaN/infinity; sending "no price" is
        // right — "$0" would not be.
        let message = report_status(&StatusReport::default().with_cost_usd(f64::NAN));
        assert_eq!(parse_status(&message).unwrap().cost, Cost::Unpriced);
    }

    #[test]
    fn unpriced_is_distinct_from_unstated_and_survives_a_merge() {
        // The wire form must carry the difference: an unpriced host says so
        // with an explicit `null`, and the panel keeps it as a fact.
        let message = report_status(&StatusReport::default().unpriced());
        let parsed = parse_status(&message).unwrap();
        assert_eq!(parsed.cost, Cost::Unpriced);
        assert!(!parsed.is_empty(), "unpriced is an assertion, not silence");

        // A later report that says nothing about cost leaves it unpriced.
        let mut cached = parsed;
        cached.merge(StatusReport::default().with_model("x"));
        assert_eq!(cached.cost, Cost::Unpriced);
        assert_eq!(Cost::Usd(1.5).usd(), Some(1.5));
        assert_eq!(Cost::Unpriced.usd(), None);
    }
}
