//! The host's choice of when the terminal asks for the user's attention.
//!
//! `polaris-tui` can ring the terminal bell and retitle the window, but only
//! for a host that turns it on (see [`NotifyConfig`]). This module is the
//! host's side of that: one setting with three values, small enough to be a
//! command-line flag and an environment variable, and the [`NotifyConfig`]
//! each value stands for.

use clap::ValueEnum;
use polaris_tui::NotifyConfig;

/// The application name that starts every notification title.
const TITLE_LABEL: &str = "polaris";

/// When the terminal asks for the user's attention.
///
/// ```
/// use clap::ValueEnum;
/// use polaris_code::terminal::notify::NotifyMode;
///
/// let mode = NotifyMode::from_str("all", true).unwrap();
/// assert!(mode.config().turn_end());
/// assert!(NotifyMode::from_str("sometimes", true).is_err());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum NotifyMode {
    /// Never ask.
    #[value(alias = "0", alias = "false")]
    Off,
    /// Ask when an approval is waiting: the agent is blocked until the user
    /// comes back.
    #[default]
    Approval,
    /// Ask when an approval is waiting, and when a turn finishes or fails.
    All,
}

impl NotifyMode {
    /// The panel configuration this mode stands for.
    #[must_use]
    pub fn config(self) -> NotifyConfig {
        let config = NotifyConfig::default().with_label(TITLE_LABEL);
        match self {
            Self::Off => config.with_approval(false),
            Self::Approval => config,
            Self::All => config.with_turn_end(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_notifies_for_approvals_only() {
        let config = NotifyMode::default().config();
        assert_eq!(NotifyMode::default(), NotifyMode::Approval);
        assert!(config.approval());
        assert!(!config.turn_end());
        assert!(config.bell() && config.title());
    }

    #[test]
    fn off_asks_for_nothing() {
        let config = NotifyMode::Off.config();
        assert!(!config.approval());
        assert!(!config.turn_end());
    }

    #[test]
    fn all_adds_the_end_of_a_turn() {
        let config = NotifyMode::All.config();
        assert!(config.approval());
        assert!(config.turn_end());
    }

    #[test]
    fn words_parse_ignoring_case() {
        for (text, mode) in [
            ("off", NotifyMode::Off),
            ("0", NotifyMode::Off),
            ("FALSE", NotifyMode::Off),
            ("approval", NotifyMode::Approval),
            ("All", NotifyMode::All),
        ] {
            assert_eq!(NotifyMode::from_str(text, true), Ok(mode), "{text:?}");
        }
        assert!(NotifyMode::from_str("sometimes", true).is_err());
    }
}
