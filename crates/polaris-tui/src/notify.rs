//! Terminal notifications — ask for the user's attention when they are not
//! looking at the screen.
//!
//! A panel that is waiting for the user (an approval, a finished turn) returns
//! a [`Notification`] from [`TuiPanel::take_notification`](crate::TuiPanel::take_notification).
//! The runtime owns stdout, so it alone writes the result: a terminal bell
//! (`BEL`) and/or a window-title change (`OSC 0`). Neither needs a desktop
//! notification service, and both work over SSH and inside `tmux`, where
//! terminals surface them as a tab badge or a flashing title.
//!
//! The title is a request, not a replacement. Before the first title change
//! the runtime asks the terminal to save the current title (xterm's title
//! stack), and restores it on the next key press, click or paste, and again
//! when the shell exits or hands the terminal to an editor. A terminal that
//! ignores the title stack still shows the notification title until the
//! application exits.
//!
//! Notifications are opt-in per panel. [`NotifyConfig`] is the shared
//! vocabulary a panel can accept from its host: which moments notify, and
//! through which channels.

use std::fmt::Write as _;

/// Longest title, in characters, the runtime will write.
const MAX_TITLE_CHARS: usize = 80;

/// Ask the terminal to save its current title on the title stack.
const PUSH_TITLE: &str = "\x1b[22;0t";
/// Ask the terminal to restore the most recently saved title.
pub(crate) const RESTORE_TITLE: &str = "\x1b[23;0t";
const BELL: &str = "\x07";

/// A request for the user's attention.
///
/// Built by a panel and delivered by the runtime. The message is shown only
/// in the window title, so keep it short; control characters and anything
/// past 80 characters are dropped when it is written.
///
/// ```
/// use polaris_tui::Notification;
///
/// let ping = Notification::new("polaris: approval needed").with_title(false);
/// assert!(ping.bell());
/// assert!(!ping.title());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    message: String,
    bell: bool,
    title: bool,
}

impl Notification {
    /// A notification that rings the bell and sets the window title to
    /// `message`.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            bell: true,
            title: true,
        }
    }

    /// Choose whether the terminal bell rings. On by default.
    #[must_use]
    pub fn with_bell(mut self, bell: bool) -> Self {
        self.bell = bell;
        self
    }

    /// Choose whether the window title shows the message. On by default.
    #[must_use]
    pub fn with_title(mut self, title: bool) -> Self {
        self.title = title;
        self
    }

    /// The text shown in the window title.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Whether the terminal bell rings.
    #[must_use]
    pub fn bell(&self) -> bool {
        self.bell
    }

    /// Whether the window title shows the message.
    #[must_use]
    pub fn title(&self) -> bool {
        self.title
    }

    /// The bytes that deliver this notification.
    ///
    /// `title_saved` says whether the runtime already asked the terminal to
    /// save the user's title, so a second notification does not push a title
    /// that is itself a notification.
    pub(crate) fn encode(&self, title_saved: bool) -> String {
        let mut out = String::new();
        let title = sanitize_title(&self.message);
        if self.title && !title.is_empty() {
            if !title_saved {
                out.push_str(PUSH_TITLE);
            }
            let _ = write!(out, "\x1b]0;{title}{BELL}");
        }
        if self.bell {
            out.push_str(BELL);
        }
        out
    }

    /// Whether delivering this notification changes the window title.
    pub(crate) fn sets_title(&self) -> bool {
        self.title && !sanitize_title(&self.message).is_empty()
    }
}

/// Drop everything a terminal could read as a command, plus bidi overrides,
/// and cap the length. The title is written between escape bytes, so a stray
/// `BEL` or `ESC` in a message would otherwise end the title early and run
/// whatever followed.
fn sanitize_title(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !ch.is_control() && !matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(MAX_TITLE_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Which moments notify, and through which channels.
///
/// The default notifies when an approval is waiting and stays quiet at the
/// end of a turn, because an approval blocks the agent until the user
/// answers, while a finished turn only needs attention if the user walked
/// away. Both channels are on.
///
/// ```
/// use polaris_tui::NotifyConfig;
///
/// let quiet_title_only = NotifyConfig::default().with_bell(false);
/// assert!(quiet_title_only.approval());
/// assert!(!quiet_title_only.turn_end());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyConfig {
    approval: bool,
    turn_end: bool,
    bell: bool,
    title: bool,
    label: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self {
            approval: true,
            turn_end: false,
            bell: true,
            title: true,
            label: "polaris-tui".to_owned(),
        }
    }
}

impl NotifyConfig {
    /// A configuration that never notifies. Useful as the "off" value of a
    /// host setting without removing the panel's notification wiring.
    #[must_use]
    pub fn off() -> Self {
        Self::default().with_approval(false).with_turn_end(false)
    }

    /// Choose whether a pending approval notifies. On by default.
    #[must_use]
    pub fn with_approval(mut self, approval: bool) -> Self {
        self.approval = approval;
        self
    }

    /// Choose whether the end of a turn notifies. Off by default.
    #[must_use]
    pub fn with_turn_end(mut self, turn_end: bool) -> Self {
        self.turn_end = turn_end;
        self
    }

    /// Choose whether the terminal bell rings. On by default.
    #[must_use]
    pub fn with_bell(mut self, bell: bool) -> Self {
        self.bell = bell;
        self
    }

    /// Choose whether the window title changes. On by default.
    #[must_use]
    pub fn with_title(mut self, title: bool) -> Self {
        self.title = title;
        self
    }

    /// The application name that starts every title, such as `polaris`.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// Whether a pending approval notifies.
    #[must_use]
    pub fn approval(&self) -> bool {
        self.approval
    }

    /// Whether the end of a turn notifies.
    #[must_use]
    pub fn turn_end(&self) -> bool {
        self.turn_end
    }

    /// Whether the terminal bell rings.
    #[must_use]
    pub fn bell(&self) -> bool {
        self.bell
    }

    /// Whether the window title changes.
    #[must_use]
    pub fn title(&self) -> bool {
        self.title
    }

    /// The notification for `what` (for example `approval needed`), through
    /// the configured channels.
    pub(crate) fn notification(&self, what: &str) -> Notification {
        Notification::new(format!("{}: {what}", self.label))
            .with_bell(self.bell)
            .with_title(self.title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_notifies_for_approvals_only_through_both_channels() {
        let config = NotifyConfig::default();
        assert!(config.approval());
        assert!(!config.turn_end());
        assert!(config.bell());
        assert!(config.title());
    }

    #[test]
    fn off_disables_both_moments() {
        let config = NotifyConfig::off();
        assert!(!config.approval());
        assert!(!config.turn_end());
    }

    #[test]
    fn the_first_title_saves_the_users_title_and_later_ones_do_not() {
        let ping = Notification::new("polaris: approval needed");
        assert_eq!(
            ping.encode(false),
            "\x1b[22;0t\x1b]0;polaris: approval needed\x07\x07"
        );
        assert_eq!(ping.encode(true), "\x1b]0;polaris: approval needed\x07\x07");
    }

    #[test]
    fn channels_are_independent() {
        let bell_only = Notification::new("x").with_title(false);
        assert_eq!(bell_only.encode(false), "\x07");
        assert!(!bell_only.sets_title());

        let title_only = Notification::new("x").with_bell(false);
        assert_eq!(title_only.encode(true), "\x1b]0;x\x07");
        assert!(title_only.sets_title());

        let silent = Notification::new("x").with_bell(false).with_title(false);
        assert_eq!(silent.encode(false), "");
    }

    #[test]
    fn a_message_cannot_end_the_title_early_or_start_another_sequence() {
        let hostile =
            Notification::new("a\x07\x1b]52;c;ZXZpbA==\x07b\u{9d}c\u{202e}d\n").encode(true);
        // Only the framing bytes remain; every control in the message is gone.
        assert_eq!(hostile, "\x1b]0;a]52;c;ZXZpbA==bcd\x07\x07");
        assert_eq!(hostile.matches('\x1b').count(), 1);
        assert_eq!(hostile.matches('\x07').count(), 2);
    }

    #[test]
    fn a_message_with_nothing_printable_sets_no_title() {
        let blank = Notification::new("\x1b\x07  \n").with_bell(false);
        assert_eq!(blank.encode(false), "");
        assert!(!blank.sets_title());
    }

    #[test]
    fn long_messages_are_capped_by_characters() {
        let long = Notification::new("é".repeat(500)).with_bell(false);
        let encoded = long.encode(true);
        let title = encoded
            .strip_prefix("\x1b]0;")
            .and_then(|rest| rest.strip_suffix('\x07'))
            .unwrap();
        assert_eq!(title.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn the_label_starts_every_title() {
        let config = NotifyConfig::default().with_label("polaris");
        assert_eq!(
            config.notification("turn finished").message(),
            "polaris: turn finished"
        );
        assert_eq!(RESTORE_TITLE, "\x1b[23;0t");
    }
}
