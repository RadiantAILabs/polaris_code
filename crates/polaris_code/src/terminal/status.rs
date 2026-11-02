//! Coding-host projection into the generic terminal status contract.
//!
//! The TUI owns presentation; this module owns the coding-specific facts it is
//! allowed to display, and states each of them in the host's own words. No
//! graph/runtime type is added.
//!
//! # The approval posture is read, not asserted
//!
//! `StatusReport::permissions` is documented in `polaris-tui` as the host's own words
//! for its approval posture — the panel keeps no permission vocabulary and no
//! mode set. The *words* are therefore chosen here, but the *posture* is not:
//! it is read from the [`PermissionPolicy`] the turn will actually resolve
//! every tool call against, via the same [`PermissionPolicy::mode_label`] that
//! the trajectory reifies for each of those decisions. The meta row and the
//! recorded provenance cannot drift apart, and a mode change takes effect on
//! the row exactly when it takes effect at the gate.
//!
//! No second vocabulary is introduced either: every sentence this module can
//! state leads with the mode's existing label.

use std::path::Path;

use polaris::plugins::UserIO;
use polaris_tui::commands::{Availability, ContextWindow, StatusReport, report_status};

use crate::agents::PermissionPolicy;
use crate::tools::IndexPhase;
use crate::usage::TokenMeter;

/// What each posture means for the two tools a permission mode governs, keyed
/// by [`PermissionPolicy::mode_label`].
///
/// Each sentence has to stay true of what the policy actually resolves, not of
/// what a mode is named after: `unattended` promises no confirmation at all,
/// because an unattended policy turns every would-be confirmation into a
/// refusal rather than into a question.
const POSTURES: [(&str, &str); 5] = [
    (
        "confirm",
        "confirm · edits and commands ask first unless a rule allows them",
    ),
    (
        "auto-edit",
        "auto-edit · edits apply unasked, commands ask first unless a rule allows them",
    ),
    (
        "auto",
        "auto · edits apply unasked, a reviewer model decides commands no rule covers",
    ),
    ("plan", "plan · edits and commands refused"),
    (
        "unattended",
        "unattended · nothing is ever asked, grants decide",
    ),
];

/// What each code-index phase means for the answers the navigation tools can
/// give about the workspace, keyed by [`IndexPhase`].
///
/// The workspace row is where this belongs: the index is a claim about *that*
/// tree, so the row that names the tree is the row that has to say whether the
/// claim is currently good. A phase whose sentence is [`None`] is one where the
/// tools answer normally and there is nothing for the user to know.
const INDEX_STATES: [(IndexPhase, Option<&str>); 4] = [
    (IndexPhase::Ready, None),
    (IndexPhase::Unstarted, Some("code index not built")),
    (IndexPhase::Building, Some("code index building")),
    (
        IndexPhase::Failed,
        Some("code index incomplete · last pass failed"),
    ),
];

/// The host's own words for what `index` means for this workspace, appended to
/// the workspace row.
fn indexing(workspace: &Path, index: IndexPhase) -> String {
    let path = workspace.display();
    match INDEX_STATES
        .iter()
        .find(|(phase, _)| *phase == index)
        .and_then(|(_, stated)| *stated)
    {
        Some(stated) => format!("{path} · {stated}"),
        None => path.to_string(),
    }
}

/// Shown when the policy reports a posture this module has no sentence for.
///
/// [`every_mode_states_its_own_posture`] proves every [`PermissionMode`]
/// reaches an entry in [`POSTURES`], so this is unreachable today. It exists so
/// that a mode added without a sentence here reads as unstated rather than as
/// the user having been promised a confirmation.
///
/// [`PermissionMode`]: crate::agents::PermissionMode
/// [`every_mode_states_its_own_posture`]: tests::every_mode_states_its_own_posture
const POSTURE_UNSTATED: &str = "permission posture unstated";

/// The host's own words for the posture `permission` puts a turn in.
///
/// Shared by the session status row and local configuration diagnostics.
#[must_use]
pub fn permission_posture(permission: &PermissionPolicy) -> &'static str {
    let label = permission.mode_label();
    POSTURES
        .iter()
        .find(|(mode, _)| *mode == label)
        .map_or(POSTURE_UNSTATED, |(_, stated)| *stated)
}

/// Send a partial session status refresh to the conversation panel.
#[expect(
    clippy::too_many_arguments,
    reason = "flat projection of independent host facts"
)]
pub async fn send(
    user_io: &UserIO,
    model: &str,
    workspace: &Path,
    session: &str,
    turn: u64,
    permission: &PermissionPolicy,
    projected_tokens: u64,
    token_budget: Option<u64>,
    compacted: bool,
    index: IndexPhase,
) {
    let report = build(
        model,
        workspace,
        session,
        turn,
        permission,
        projected_tokens,
        token_budget,
        compacted,
        index,
    );
    let _ = user_io.send(report_status(&report)).await;
}

#[expect(
    clippy::too_many_arguments,
    reason = "flat projection of independent host facts"
)]
fn build(
    model: &str,
    workspace: &Path,
    session: &str,
    turn: u64,
    permission: &PermissionPolicy,
    projected_tokens: u64,
    token_budget: Option<u64>,
    compacted: bool,
    index: IndexPhase,
) -> StatusReport {
    let mut window = ContextWindow::new(projected_tokens)
        .estimated(true)
        .compacting(compacted);
    if let Some(limit) = token_budget {
        window = window.with_limit(limit);
    }
    StatusReport::default()
        .with_model(model)
        .with_workspace(indexing(workspace, index))
        .with_session(format!("{session} · turn {turn}"))
        .with_permissions(permission_posture(permission))
        .with_context(window)
        .with_availability(Availability::Available)
}

/// Send the latest host-priced cumulative session cost.
pub async fn send_cost(user_io: &UserIO, model: &str, meter: &TokenMeter) {
    let report = match meter.estimated_cost_usd(model) {
        Some(cost) => StatusReport::default().with_cost_usd(cost),
        None => StatusReport::default().unpriced(),
    };
    let _ = user_io.send(report_status(&report)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{PermissionMode, UnattendedGrants};
    use polaris_tui::commands::Cost;

    /// Every mode a host can put a turn in, including both shapes of unattended
    /// run: one that was granted something, and one that was granted nothing.
    fn every_mode() -> [PermissionMode; 6] {
        [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Auto,
            PermissionMode::Plan,
            PermissionMode::Unattended(UnattendedGrants::default()),
            PermissionMode::Unattended(UnattendedGrants {
                edit_file: true,
                run_command_patterns: vec!["ls *".to_string()],
            }),
        ]
    }

    /// Totality over [`PermissionMode`], which the table lookup cannot get from
    /// the compiler: a mode added without a sentence in [`POSTURES`] fails here
    /// instead of silently reaching [`POSTURE_UNSTATED`] on a user's meta row.
    #[test]
    fn every_mode_states_its_own_posture() {
        for mode in every_mode() {
            let stated = permission_posture(&mode.permission_policy());
            assert_ne!(
                stated,
                POSTURE_UNSTATED,
                "`{}` has no posture sentence in POSTURES",
                mode.label()
            );
            assert!(
                stated.contains(mode.label()),
                "a posture has to name its own mode; `{}` states {stated:?}",
                mode.label()
            );
        }
    }

    /// Two modes permit different things, so they may not read the same. The
    /// two unattended grant shapes are deliberately excluded: they share a
    /// posture because neither one ever asks.
    #[test]
    fn modes_that_permit_different_things_read_differently() {
        let stated: Vec<&str> = [
            PermissionMode::Confirm,
            PermissionMode::AutoEdit,
            PermissionMode::Auto,
            PermissionMode::Plan,
            PermissionMode::Unattended(UnattendedGrants::default()),
        ]
        .iter()
        .map(|mode| permission_posture(&mode.permission_policy()))
        .collect();

        let mut unique = stated.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            stated.len(),
            "two postures collided: {stated:?}"
        );
    }

    /// The honesty the whole projection exists for. An unattended policy turns
    /// every would-be confirmation into a refusal — `resolve` never returns
    /// `Confirm` under it — so its posture must not offer the user one,
    /// whatever it was granted.
    #[test]
    fn an_unattended_posture_never_promises_a_confirmation() {
        for mode in every_mode()
            .into_iter()
            .filter(|mode| matches!(mode, PermissionMode::Unattended(_)))
        {
            let stated = permission_posture(&mode.permission_policy());
            assert!(
                !stated.contains("confirm"),
                "an unattended run asks no one anything: {stated:?}"
            );
            assert!(
                !stated.contains("ask first"),
                "an unattended run asks no one anything: {stated:?}"
            );
        }
    }

    /// A workspace row with an index behind it. The same three cases the tools
    /// have to keep apart have to be visible on screen too: a user watching the
    /// agent answer "no such symbol" should be able to see whether the index
    /// was even in a position to know.
    #[test]
    fn the_workspace_row_says_whether_its_index_can_be_trusted() {
        let path = Path::new("/repo");
        let ready = indexing(path, IndexPhase::Ready);
        assert_eq!(
            ready, "/repo",
            "a ready index is the ordinary case and says nothing extra"
        );

        let mut stated = vec![ready];
        for phase in [
            IndexPhase::Unstarted,
            IndexPhase::Building,
            IndexPhase::Failed,
        ] {
            let row = indexing(path, phase);
            assert!(
                row.starts_with("/repo · "),
                "the note belongs to the workspace it is about: {row:?}"
            );
            assert!(
                row.contains("index"),
                "a user cannot act on a note that does not name what is wrong: {row:?}"
            );
            stated.push(row);
        }

        let mut unique = stated.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(
            unique.len(),
            stated.len(),
            "two index phases read the same on the workspace row: {stated:?}"
        );
    }

    /// Totality over [`IndexPhase`], which the table lookup cannot get from the
    /// compiler: a phase added without an entry in [`INDEX_STATES`] would
    /// silently read as a healthy index.
    #[test]
    fn every_index_phase_has_an_entry() {
        for phase in [
            IndexPhase::Unstarted,
            IndexPhase::Building,
            IndexPhase::Ready,
            IndexPhase::Failed,
        ] {
            assert!(
                INDEX_STATES.iter().any(|(known, _)| *known == phase),
                "{phase:?} has no entry in INDEX_STATES"
            );
        }
    }

    /// The status row is the *same* fact the tools gate on, not a second copy
    /// of it: a report built while the index is building has to show it.
    #[test]
    fn a_building_index_reaches_the_report() {
        let report = build(
            "provider/model",
            Path::new("/repo"),
            "session-a",
            1,
            &PermissionMode::Confirm.permission_policy(),
            0,
            None,
            false,
            IndexPhase::Building,
        );
        let row = report
            .workspace
            .expect("the workspace row is always stated");
        assert!(row.starts_with("/repo"), "got: {row:?}");
        assert!(
            row.contains("code index building"),
            "the report dropped what the workspace row knows about its index: {row:?}"
        );
    }

    #[test]
    fn report_keeps_projection_separate_and_unknown_price_truthful() {
        let report = build(
            "provider/unknown",
            Path::new("/repo"),
            "session-a",
            3,
            &PermissionMode::Confirm.permission_policy(),
            48_000,
            Some(120_000),
            true,
            IndexPhase::Ready,
        );
        let window = report.context.unwrap();
        assert_eq!(window.tokens, 48_000);
        assert_eq!(window.limit, Some(120_000));
        assert!(window.estimated);
        assert!(window.compacting);
        assert_eq!(report.cost, Cost::Unstated);
        assert_eq!(report.session.as_deref(), Some("session-a · turn 3"));
        assert_eq!(
            report.permissions.as_deref(),
            Some(permission_posture(
                &PermissionMode::Confirm.permission_policy()
            ))
        );

        let meter = TokenMeter::new();
        meter.add(900, 100);
        let cost = match meter.estimated_cost_usd("provider/unknown") {
            Some(value) => StatusReport::default().with_cost_usd(value),
            None => StatusReport::default().unpriced(),
        };
        assert_eq!(cost.cost, Cost::Unpriced);
    }
}
