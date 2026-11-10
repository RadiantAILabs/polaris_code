//! Modal choice lists and text fields for host workflows.
//!
//! Hosts await answers through [`DialogHost`], outside agent IO and transcript
//! history. Dropping the pending request closes the dialog and discards its input.

use crate::TuiEvent;
use crate::composer::Composer;
use crate::stray::StrayGuard;
use crate::text::safe_text as safe;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Text;
use ratatui::widgets::ListState;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;
use tokio::sync::oneshot;

mod panel;
mod render;
pub use panel::DialogPanel;

const MAX_INPUT_BYTES: usize = 16 * 1024;
/// Rows one page key or wheel step scrolls.
const SCROLL_ROWS: u16 = 3;

/// A choice label, description, and optional shortcut.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct DialogChoice {
    /// Label shown after the choice number, starting at 1.
    pub label: String,
    /// Dimmed text below the label; omitted on short screens or when empty.
    pub description: String,
    /// Optional key to select this choice.
    ///
    /// ASCII letters match either case. Digits select choices by number.
    /// If choices share a shortcut, the first one is selected.
    pub shortcut: Option<char>,
}

impl DialogChoice {
    /// Create a choice with a label and description.
    #[must_use]
    pub fn new(label: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            shortcut: None,
        }
    }

    /// Set the key that selects this choice.
    #[must_use]
    pub fn with_shortcut(mut self, shortcut: char) -> Self {
        self.shortcut = Some(shortcut);
        self
    }
}

/// Text below the controls that users can select and copy, such as a URL.
///
/// The value is omitted from debug output.
#[non_exhaustive]
pub struct DialogDetail {
    /// Heading above the value.
    pub label: String,
    /// Text to display, with control characters removed when rendered.
    pub value: String,
    /// Choice to activate when the displayed detail is clicked, if any.
    pub choice: Option<usize>,
}

impl std::fmt::Debug for DialogDetail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogDetail")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

/// A choice list or text field.
#[non_exhaustive]
pub enum DialogContent {
    /// A list of choices; the answer is the selected index, starting at 0.
    /// An empty list shows a message that the user can dismiss with Esc or Ctrl-C.
    Choices(Vec<DialogChoice>),
    /// A text field with optional masking and a validation message.
    Input {
        /// Initial field contents. Do not prefill saved credentials.
        ///
        /// Control characters are removed and the text is cut to the field's
        /// size limit, as for pasted text.
        initial: String,
        /// Mask the entered text.
        secret: bool,
        /// Validation message below the field. To retry without losing entered
        /// text, pass that text as `initial` in the next request.
        error: Option<String>,
    },
}

/// A modal prompt with a choice list or text field.
///
/// The host interprets the answer and performs the selected action. Debug
/// output shows labels (the title, choices and detail label) and omits text
/// that may carry secrets or URLs: the body, input values, validation
/// messages and the detail value.
///
/// ```
/// use polaris_tui::dialogs::{Dialog, DialogChoice, DialogContent};
/// use polaris_tui::ratatui::text::Text;
///
/// let dialog = Dialog::new("Export", "Choose an output format", DialogContent::Choices(vec![
///     DialogChoice::new("JSON", "Structured output").with_shortcut('j'),
///     DialogChoice::new("Text", "Plain output").with_shortcut('t'),
/// ]))
/// .with_header([Text::from("Workspace tools")])
/// .with_detail("Destination", "reports/latest");
/// ```
#[non_exhaustive]
pub struct Dialog {
    /// Dialog heading.
    pub title: String,
    /// Wrapped text below the title; PgUp and PgDn scroll it.
    pub body: String,
    /// Choice list or text field to show.
    pub content: DialogContent,
    /// Headers to try in order. The first that fits above the title is shown;
    /// none is shown when the controls need the space.
    pub header: Vec<Text<'static>>,
    /// Text below the controls that users can select and copy.
    pub detail: Option<DialogDetail>,
}

impl Dialog {
    /// Create a dialog without a header or detail text.
    #[must_use]
    pub fn new(title: impl Into<String>, body: impl Into<String>, content: DialogContent) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            content,
            header: Vec::new(),
            detail: None,
        }
    }

    /// Activate a choice immediately when the detail text is clicked.
    /// Call after [`Self::with_detail`]. Invalid choice indices are ignored.
    /// The host must enable mouse capture to receive clicks; keyboard shortcuts
    /// remain available when the user chooses native terminal selection.
    #[must_use]
    pub fn with_detail_choice(mut self, index: usize) -> Self {
        if let Some(detail) = &mut self.detail {
            detail.choice = Some(index);
        }
        self
    }

    /// Set the headers to try, in preference order.
    #[must_use]
    pub fn with_header(mut self, alternatives: impl IntoIterator<Item = Text<'static>>) -> Self {
        self.header = alternatives.into_iter().collect();
        self
    }

    /// Add text below the controls that users can select and copy.
    #[must_use]
    pub fn with_detail(mut self, label: impl Into<String>, value: impl Into<String>) -> Self {
        self.detail = Some(DialogDetail {
            label: label.into(),
            value: value.into(),
            choice: None,
        });
        self
    }
}

/// A dialog answer. Entered text is redacted in debug output.
#[non_exhaustive]
pub enum DialogAnswer {
    /// Selected choice index, starting at 0.
    Choice(usize),
    /// Entered text.
    Input(String),
}

impl std::fmt::Debug for DialogContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Choices(choices) => f.debug_tuple("Choices").field(choices).finish(),
            Self::Input { secret, error, .. } => f
                .debug_struct("Input")
                .field("secret", secret)
                .field("error", &error.is_some())
                .finish_non_exhaustive(),
        }
    }
}

impl std::fmt::Debug for Dialog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dialog")
            .field("title", &self.title)
            .field("content", &self.content)
            .field("detail", &self.detail)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for DialogAnswer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Choice(index) => f.debug_tuple("Choice").field(index).finish(),
            Self::Input(_) => f.write_str("Input([redacted])"),
        }
    }
}

/// The open dialog and its view state.
struct Active {
    dialog: Dialog,
    composer: Composer,
    list: ListState,
    body_scroll: u16,
    detail_scroll: u16,
    /// Last rendered value bounds, used to activate an optional detail choice.
    detail_area: Option<Rect>,
    /// Whether the last frame cut the body short, so page keys scroll it.
    body_overflows: bool,
    guard: StrayGuard,
    answer: Option<oneshot::Sender<DialogAnswer>>,
}

impl Active {
    fn key(&mut self, key: KeyEvent, now: Instant) -> Option<DialogAnswer> {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::PageUp => return self.scroll(false, shift),
            KeyCode::PageDown => return self.scroll(true, shift),
            _ => {}
        }
        let press = key.kind == KeyEventKind::Press;
        match &self.dialog.content {
            DialogContent::Choices(choices) => {
                let count = choices.len();
                let selected = self.list.selected().unwrap_or(0);
                let plain = !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                match key.code {
                    KeyCode::Up if count > 0 => self
                        .list
                        .select(Some(selected.checked_sub(1).unwrap_or(count - 1))),
                    KeyCode::Down if count > 0 => self.list.select(Some((selected + 1) % count)),
                    KeyCode::Enter if press && count > 0 => {
                        return self.answer(DialogAnswer::Choice(selected), now);
                    }
                    KeyCode::Char(character) if plain => {
                        match choice_index(choices, character).filter(|_| press) {
                            Some(index) => return self.answer(DialogAnswer::Choice(index), now),
                            None => self.guard.stray(now),
                        }
                    }
                    _ => {}
                }
            }
            DialogContent::Input { .. } if key.code == KeyCode::Enter => {
                if press {
                    let text = self.composer.text().to_owned();
                    return self.answer(DialogAnswer::Input(text), now);
                }
            }
            DialogContent::Input { .. } => {
                self.composer.edit_line(key, MAX_INPUT_BYTES);
            }
        }
        None
    }

    /// Scroll the detail when requested or when the body fits; otherwise scroll the body.
    fn scroll(&mut self, down: bool, detail: bool) -> Option<DialogAnswer> {
        let offset = if detail || !self.body_overflows {
            &mut self.detail_scroll
        } else {
            &mut self.body_scroll
        };
        *offset = if down {
            offset.saturating_add(SCROLL_ROWS)
        } else {
            offset.saturating_sub(SCROLL_ROWS)
        };
        None
    }

    fn mouse(&mut self, mouse: &MouseEvent) -> Option<DialogAnswer> {
        let detail = mouse.modifiers.contains(KeyModifiers::SHIFT);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll(false, detail),
            MouseEventKind::ScrollDown => self.scroll(true, detail),
            MouseEventKind::Down(MouseButton::Left)
                if mouse.modifiers.is_empty()
                    && self
                        .detail_area
                        .is_some_and(|area| area.contains((mouse.column, mouse.row).into())) =>
            {
                let index = self.dialog.detail.as_ref()?.choice?;
                match &self.dialog.content {
                    DialogContent::Choices(choices) if index < choices.len() => {
                        Some(DialogAnswer::Choice(index))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn paste(&mut self, text: &str) {
        if matches!(self.dialog.content, DialogContent::Input { .. }) {
            let room = MAX_INPUT_BYTES.saturating_sub(self.composer.text().len());
            self.composer.insert_paste(&field_text(text, room));
        }
    }

    /// Give `answer` unless the key arrived too soon after a stray one.
    fn answer(&mut self, answer: DialogAnswer, now: Instant) -> Option<DialogAnswer> {
        self.guard.admits(now).then_some(answer)
    }
}

fn choice_index(choices: &[DialogChoice], character: char) -> Option<usize> {
    if let Some(digit) = character.to_digit(10) {
        return (digit as usize)
            .checked_sub(1)
            .filter(|index| *index < choices.len());
    }
    choices.iter().position(|choice| {
        choice
            .shortcut
            .is_some_and(|shortcut| shortcut.eq_ignore_ascii_case(&character))
    })
}

/// Text for the input field: control characters removed and cut to `room` bytes.
fn field_text(text: &str, mut room: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take_while(|character| {
            let bytes = character.len_utf8();
            let fits = bytes <= room;
            room = room.saturating_sub(bytes);
            fits
        })
        .collect()
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    /// Last page held while a workflow prepares its next dialog.
    retained: Option<Active>,
    workflows: usize,
    /// Last key press, used to guard answers when the next dialog opens.
    last_key: Option<Instant>,
}

impl State {
    fn finish(&mut self, answer: Option<DialogAnswer>) {
        if let Some(mut active) = self.active.take() {
            if let Some(sender) = active.answer.take()
                && let Some(answer) = answer
            {
                let _ = sender.send(answer);
            }
            if self.workflows > 0 {
                self.retained = Some(active);
            }
        }
    }
}

/// Keeps the last dialog visible between pages of an asynchronous workflow.
/// Dropping the guard releases the retained page. It does not dismiss a pending
/// dialog; dropping that dialog's [`DialogHost::present`] future does.
#[must_use = "keep the guard alive for the duration of the dialog workflow"]
pub struct DialogWorkflow {
    host: DialogHost,
}

impl std::fmt::Debug for DialogWorkflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogWorkflow")
            .field("host", &self.host)
            .finish()
    }
}

impl Drop for DialogWorkflow {
    fn drop(&mut self) {
        let mut state = self.host.state();
        state.workflows -= 1;
        if state.workflows == 0 {
            state.retained = None;
        }
    }
}

/// Connects asynchronous host prompts to a terminal panel.
///
/// Share a clone with the panel through [`wrap`](Self::wrap), or through
/// `ConversationPanelPlugin::with_dialogs` when built-in panels are enabled.
/// Run the panel while the host awaits [`present`](Self::present).
///
/// One key press answers a choice, so keys typed into the panel just before a
/// dialog opens must not answer it. Enter, digits and shortcuts require 400 ms
/// of quiet after keys from before the dialog opened, unrecognized characters,
/// or refused answers. Moving, scrolling, editing and cancelling always work.
///
/// ```
/// use polaris_tui::TuiEvent;
/// use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
/// use polaris_tui::dialogs::{Dialog, DialogAnswer, DialogChoice, DialogContent, DialogHost};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// # tokio::runtime::Builder::new_current_thread().build()?.block_on(async {
///
/// let dialogs = DialogHost::default();
/// // The terminal feeds input to `dialogs` while the host awaits the answer.
/// let request = tokio::spawn({
///     let dialogs = dialogs.clone();
///     async move {
///         dialogs
///             .present(Dialog::new(
///                 "Choose a connection",
///                 "",
///                 DialogContent::Choices(vec![
///                     DialogChoice::new("Local", "Use this machine"),
///                     DialogChoice::new("Remote", "Use the shared server"),
///                 ]),
///             ))
///             .await
///     }
/// });
/// tokio::task::yield_now().await;
///
/// dialogs.handle_event(&TuiEvent::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)));
/// assert!(matches!(request.await?, Some(DialogAnswer::Choice(1))));
/// # Ok::<_, Box<dyn std::error::Error>>(())
/// # })
/// # }
/// ```
#[derive(Clone, Default)]
pub struct DialogHost {
    state: Arc<Mutex<State>>,
    serial: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for DialogHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DialogHost")
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl DialogHost {
    /// Keep the previous page visible while preparing the next dialog.
    /// Hold the returned guard for the complete workflow. Input received in a
    /// gap is consumed so it cannot reach the underlying panel.
    pub fn begin_workflow(&self) -> DialogWorkflow {
        self.state().workflows += 1;
        DialogWorkflow { host: self.clone() }
    }

    /// Wait for a dialog answer. Requests are shown one at a time.
    /// Returns `None` on cancellation; dropping this future dismisses its dialog.
    /// Within [`Self::begin_workflow`], the last page remains visible without
    /// accepting answers until its replacement is ready or the workflow ends.
    pub async fn present(&self, dialog: Dialog) -> Option<DialogAnswer> {
        let _serial = self.serial.lock().await;
        let (answer, receive) = oneshot::channel();
        let mut composer = Composer::new();
        if let DialogContent::Input { initial, .. } = &dialog.content {
            composer.insert_paste(&field_text(initial, MAX_INPUT_BYTES));
        }
        {
            let mut state = self.state();
            let guard = StrayGuard::new(state.last_key);
            state.retained = None;
            state.active = Some(Active {
                dialog,
                composer,
                list: ListState::default().with_selected(Some(0)),
                body_scroll: 0,
                detail_scroll: 0,
                detail_area: None,
                body_overflows: false,
                guard,
                answer: Some(answer),
            });
        }
        struct Dismiss<'a>(&'a DialogHost);
        impl Drop for Dismiss<'_> {
            fn drop(&mut self) {
                self.0.state().finish(None);
            }
        }
        // Clear this dialog before releasing the next request.
        let _dismiss = Dismiss(self);
        receive.await.ok()
    }

    pub(crate) fn is_active(&self) -> bool {
        let state = self.state();
        state.active.is_some() || state.retained.is_some()
    }

    /// Handle input for the open dialog and return whether it was consumed.
    ///
    /// Esc and Ctrl-C cancel the dialog. Pass key events while no dialog is
    /// open too: they are not consumed, but let the next dialog ignore answer
    /// keys typed just before it opened.
    pub fn handle_event(&self, event: &TuiEvent) -> bool {
        self.handle_event_at(event, Instant::now())
    }

    fn handle_event_at(&self, event: &TuiEvent, now: Instant) -> bool {
        let mut state = self.state();
        if let TuiEvent::Key(key) = event
            && key.kind != KeyEventKind::Release
        {
            state.last_key = Some(now);
        }
        let Some(active) = state.active.as_mut() else {
            return state.retained.is_some()
                && matches!(
                    event,
                    TuiEvent::Key(_) | TuiEvent::Paste(_) | TuiEvent::Mouse(_)
                );
        };
        let answer = match event {
            TuiEvent::Key(key) if key.kind == KeyEventKind::Release => None,
            TuiEvent::Key(key)
                if key.code == KeyCode::Esc
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')) =>
            {
                state.finish(None);
                return true;
            }
            TuiEvent::Key(key) => active.key(*key, now),
            TuiEvent::Paste(text) => {
                active.paste(text);
                None
            }
            TuiEvent::Mouse(mouse) => active.mouse(mouse),
            _ => return false,
        };
        if let Some(answer) = answer {
            state.finish(Some(answer));
        }
        true
    }

    /// Draw the open dialog or retained workflow page.
    /// Returns `false` when neither is visible.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) -> bool {
        let mut state = self.state();
        let State {
            active, retained, ..
        } = &mut *state;
        let Some(active) = active.as_mut().or(retained.as_mut()) else {
            return false;
        };
        render::dialog(active, frame, area);
        true
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|err| err.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stray::QUIET;
    use ratatui::style::{Color, Style};
    use ratatui::{Terminal, backend::TestBackend};
    use tokio::task::JoinHandle;

    /// Present `dialog` on a background task and wait until it is open.
    async fn open(dialog: Dialog) -> (DialogHost, JoinHandle<Option<DialogAnswer>>) {
        let host = DialogHost::default();
        let task = tokio::spawn({
            let host = host.clone();
            async move { host.present(dialog).await }
        });
        wait_for(|| host.state().active.is_some()).await;
        (host, task)
    }

    async fn wait_for(condition: impl Fn() -> bool) {
        for _ in 0..100 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            condition(),
            "dialog reaches the expected state within 100 yields"
        );
    }

    fn choices(labels: &[(&str, &str, Option<char>)]) -> DialogContent {
        DialogContent::Choices(
            labels
                .iter()
                .map(|(label, description, shortcut)| {
                    let choice = DialogChoice::new(*label, *description);
                    match shortcut {
                        Some(key) => choice.with_shortcut(*key),
                        None => choice,
                    }
                })
                .collect(),
        )
    }

    fn input(initial: impl Into<String>, secret: bool, error: Option<&str>) -> DialogContent {
        DialogContent::Input {
            initial: initial.into(),
            secret,
            error: error.map(Into::into),
        }
    }

    fn key(code: KeyCode) -> TuiEvent {
        TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn draw(host: &DialogHost, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal =
            Terminal::new(TestBackend::new(width, height)).expect("test terminal is created");
        terminal
            .draw(|frame| {
                host.render(frame, frame.area());
            })
            .expect("dialog draws");
        terminal
    }

    fn screen(host: &DialogHost, width: u16, height: u16) -> Vec<String> {
        draw(host, width, height)
            .backend()
            .buffer()
            .content()
            .chunks(usize::from(width.max(1)))
            .map(|row| row.iter().map(|cell| cell.symbol()).collect())
            .collect()
    }

    #[tokio::test]
    async fn displayed_dialog_text_removes_bidi_and_invisible_controls() {
        let (host, task) = open(
            Dialog::new(
                "Ti\u{202e}tle",
                "Bo\u{200b}dy",
                choices(&[("Ch\u{2066}oice", "De\u{feff}scription", None)]),
            )
            .with_header([Text::from("He\u{202a}ader")])
            .with_detail("Li\u{2069}nk", "https://exa\u{200b}mple.com/"),
        )
        .await;
        let displayed = screen(&host, 80, 30).join("\n");
        for expected in [
            "Title",
            "Body",
            "Choice",
            "Description",
            "Header",
            "Link",
            "https://example.com/",
        ] {
            assert!(
                displayed.contains(expected),
                "sanitized {expected} is visible: {displayed}"
            );
        }
        for hidden in [
            '\u{202e}', '\u{200b}', '\u{2066}', '\u{feff}', '\u{202a}', '\u{2069}',
        ] {
            assert!(!displayed.contains(hidden), "display excludes {hidden:?}");
        }
        host.handle_event(&key(KeyCode::Esc));
        assert!(
            task.await.expect("dialog cancels").is_none(),
            "Esc dismisses the dialog"
        );
    }

    #[test]
    fn debug_shows_labels_and_redacts_values() {
        let dialog = Dialog::new(
            "shown-title",
            "private-body",
            DialogContent::Input {
                initial: "private-key".into(),
                secret: true,
                error: Some("private-error".into()),
            },
        )
        .with_header([Text::from("private-header")])
        .with_detail("shown-label", "https://example.com/?token=private-token");
        let choice = Dialog::new(
            "shown-title",
            "private-body",
            choices(&[("shown-choice", "shown-description", None)]),
        );
        let answer = DialogAnswer::Input("private-answer".into());
        let shown = format!("{dialog:?} {choice:?} {answer:?}");
        assert!(!shown.contains("private-"), "values are redacted: {shown}");
        for label in [
            "shown-title",
            "shown-label",
            "shown-choice",
            "shown-description",
        ] {
            assert!(shown.contains(label), "{label} is shown: {shown}");
        }
    }

    #[tokio::test]
    async fn input_filters_and_bounds_initial_paste_and_typing() {
        let initial = format!("\u{1b}{}", "a".repeat(1023));
        let (host, task) = open(Dialog::new("Key", "", input(initial, true, None))).await;
        assert!(!host.handle_event(&TuiEvent::Tick), "ticks pass through");
        assert!(
            !host.handle_event(&TuiEvent::Resize { cols: 80, rows: 24 }),
            "resizes pass through"
        );
        // 1023 bytes leave room for 7680 two-byte characters and one byte.
        host.handle_event(&TuiEvent::Paste("é".repeat(MAX_INPUT_BYTES)));
        for character in ['é', 'b', 'c'] {
            host.handle_event(&key(KeyCode::Char(character)));
        }
        host.handle_event(&key(KeyCode::Enter));
        let Some(DialogAnswer::Input(value)) = task.await.expect("dialog task completes") else {
            panic!("expected an input answer");
        };
        assert_eq!(
            value.len(),
            MAX_INPUT_BYTES,
            "value fills the budget exactly"
        );
        assert!(
            value.starts_with(&"a".repeat(1023)),
            "initial control characters are removed"
        );
        assert_eq!(
            value[1023..].chars().filter(|c| *c == 'é').count(),
            (MAX_INPUT_BYTES - 1024) / 2,
            "paste stops at a character boundary"
        );
        assert!(value.ends_with('b'), "only the one-byte character fits");
    }

    #[tokio::test]
    async fn input_uses_line_editing_keys() {
        let (host, task) = open(Dialog::new("Name", "", input("alpha beta", false, None))).await;
        host.handle_event(&key(KeyCode::Home));
        host.handle_event(&key(KeyCode::Delete));
        host.handle_event(&TuiEvent::Key(KeyEvent::new(
            KeyCode::Right,
            KeyModifiers::ALT,
        )));
        host.handle_event(&TuiEvent::Key(KeyEvent::new(
            KeyCode::Char('w'),
            KeyModifiers::CONTROL,
        )));
        host.handle_event(&key(KeyCode::Char('x')));
        host.handle_event(&key(KeyCode::Enter));
        assert!(
            matches!(task.await.expect("dialog task completes"), Some(DialogAnswer::Input(value)) if value == "x beta"),
            "Delete, word motion and word deletion edit the field"
        );
    }

    #[tokio::test]
    async fn choices_highlight_only_the_selected_label() {
        let (host, task) = open(Dialog::new(
            "Connect",
            "",
            choices(&[
                ("Workspace", "Use the shared workspace", None),
                ("Local", "Use this machine", None),
            ]),
        ))
        .await;
        for selected in ["1. Workspace", "2. Local"] {
            let terminal = draw(&host, 80, 24);
            let buffer = terminal.backend().buffer();
            let mut label_seen = false;
            let mut descriptions_seen = 0;
            for y in 0..24 {
                let text: String = (0..80).map(|x| buffer[(x, y)].symbol()).collect();
                let column = |byte| unicode_width::UnicodeWidthStr::width(&text[..byte]) as u16;
                if let Some(byte) = text.find(selected) {
                    assert_eq!(
                        buffer[(column(byte), y)].fg,
                        Color::Cyan,
                        "{selected} is highlighted"
                    );
                    label_seen = true;
                }
                for description in ["Use the shared workspace", "Use this machine"] {
                    if let Some(byte) = text.find(description) {
                        assert_eq!(
                            buffer[(column(byte), y)].fg,
                            Color::DarkGray,
                            "{description} stays dimmed"
                        );
                        descriptions_seen += 1;
                    }
                }
            }
            assert!(label_seen, "selected label is visible");
            assert_eq!(descriptions_seen, 2, "both descriptions are visible");
            host.handle_event(&key(KeyCode::Down));
        }
        host.handle_event(&key(KeyCode::Esc));
        assert!(
            task.await.expect("dialog task completes").is_none(),
            "Esc cancels"
        );
    }

    #[tokio::test]
    async fn choices_wrap_without_losing_the_ending() {
        let (host, task) = open(Dialog::new(
            "Choose an account or connection",
            "Review before continuing.",
            choices(&[(
                "Connect to the development workspace account",
                "This connection uses the development account billing allowance",
                None,
            )]),
        ))
        .await;
        let text = screen(&host, 24, 24).join("\n");
        assert!(text.contains("account"), "label ending is visible: {text}");
        assert!(
            text.contains("allowance"),
            "description ending is visible: {text}"
        );
        host.handle_event(&key(KeyCode::Enter));
        assert!(
            matches!(
                task.await.expect("dialog task completes"),
                Some(DialogAnswer::Choice(0))
            ),
            "Enter selects the highlighted choice"
        );
    }

    #[tokio::test]
    async fn choices_answer_by_digit_or_shortcut_only() {
        let (host, task) = open(Dialog::new(
            "Export",
            "",
            choices(&[("Save", "", Some('s')), ("Preview", "", Some('p'))]),
        ))
        .await;
        let t0 = Instant::now();
        for event in [
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            KeyEvent::new_with_kind(KeyCode::Char('p'), KeyModifiers::NONE, KeyEventKind::Repeat),
            KeyEvent::new(KeyCode::Char('9'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        ] {
            host.handle_event_at(&TuiEvent::Key(event), t0);
            assert!(host.is_active(), "{event:?} does not answer");
        }
        host.handle_event_at(&key(KeyCode::Char('P')), t0 + QUIET);
        assert!(
            matches!(
                task.await.expect("dialog task completes"),
                Some(DialogAnswer::Choice(1))
            ),
            "shortcuts match either case"
        );
    }

    #[tokio::test]
    async fn choices_ignore_answer_keys_typed_before_open() {
        let t0 = Instant::now();
        let host = DialogHost::default();
        assert!(
            !host.handle_event_at(&key(KeyCode::Char('x')), t0),
            "keys pass through while no dialog is open"
        );
        let task = tokio::spawn({
            let host = host.clone();
            async move {
                host.present(Dialog::new(
                    "Choose",
                    "",
                    choices(&[("First", "", None), ("Second", "", None)]),
                ))
                .await
            }
        });
        wait_for(|| host.state().active.is_some()).await;
        let at = |ms| t0 + std::time::Duration::from_millis(ms);
        host.handle_event_at(&key(KeyCode::Char('1')), at(100));
        host.handle_event_at(&key(KeyCode::Enter), at(300));
        assert!(
            host.is_active(),
            "answer keys within the quiet window are ignored"
        );
        host.handle_event_at(&key(KeyCode::Down), at(310));
        host.handle_event_at(&key(KeyCode::Enter), at(300) + QUIET);
        assert!(
            matches!(
                task.await.expect("dialog task completes"),
                Some(DialogAnswer::Choice(1))
            ),
            "moving works at once and Enter answers after the window"
        );
    }

    #[tokio::test]
    async fn answer_key_guards_the_next_dialog() {
        let (host, first) = open(Dialog::new(
            "Account",
            "",
            choices(&[("Keep connection", "", None), ("Sign out", "", None)]),
        ))
        .await;
        let at = Instant::now();
        host.handle_event_at(&key(KeyCode::Char('2')), at);
        assert!(
            matches!(
                first.await.expect("first dialog completes"),
                Some(DialogAnswer::Choice(1))
            ),
            "the first key selects sign-out"
        );
        let confirm = tokio::spawn({
            let host = host.clone();
            async move {
                host.present(Dialog::new(
                    "Confirm sign-out",
                    "",
                    choices(&[("Keep connection", "", None), ("Sign out", "", None)]),
                ))
                .await
            }
        });
        wait_for(|| host.state().active.is_some()).await;
        let repeated = at + std::time::Duration::from_millis(100);
        host.handle_event_at(&key(KeyCode::Char('2')), repeated);
        assert!(
            host.is_active(),
            "a rapid second press cannot confirm sign-out"
        );
        host.handle_event_at(&key(KeyCode::Char('2')), repeated + QUIET);
        assert!(
            matches!(
                confirm.await.expect("confirmation completes"),
                Some(DialogAnswer::Choice(1))
            ),
            "a deliberate press after the quiet window confirms"
        );
    }

    #[tokio::test]
    async fn page_keys_scroll_the_overflowing_body_then_the_detail() {
        let numbered = |label, count| {
            (1..=count)
                .map(|line| format!("{label} {line:02}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let (host, task) = open(
            Dialog::new(
                "Review",
                numbered("Body", 20),
                choices(&[("Done", "", None)]),
            )
            .with_detail("Notes", numbered("Detail", 80)),
        )
        .await;
        let before = screen(&host, 40, 16).join("\n");
        assert!(
            before.contains("Body 01"),
            "body starts at the first line: {before}"
        );
        assert!(!before.contains("Body 20"), "body overflows: {before}");
        host.handle_event(&key(KeyCode::PageDown));
        let after = screen(&host, 40, 16).join("\n");
        assert!(
            !after.contains("Body 01"),
            "PgDn hides the first body line: {after}"
        );
        assert_eq!(
            after
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Body ")),
            Some("Body 04"),
            "PgDn advances three body lines: {after}"
        );
        host.handle_event(&TuiEvent::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::SHIFT,
        )));
        host.handle_event(&TuiEvent::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }));
        let after = screen(&host, 40, 16).join("\n");
        assert!(
            after.contains("Body 01"),
            "wheel restores the first body line: {after}"
        );
        let before = screen(&host, 120, 60).join("\n");
        assert!(
            before.contains("Body 20"),
            "the taller screen fits the body: {before}"
        );
        assert!(
            !before.contains("Detail 01"),
            "Shift+PgDn scrolled the detail: {before}"
        );
        assert_eq!(
            before
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Detail ")),
            Some("Detail 04"),
            "detail starts three lines down: {before}"
        );
        host.handle_event(&key(KeyCode::PageDown));
        let after = screen(&host, 120, 60).join("\n");
        assert!(
            after.contains("Body 01"),
            "body stays at the start: {after}"
        );
        assert!(
            !after.contains("Detail 04"),
            "PgDn hides the previous detail line: {after}"
        );
        assert_eq!(
            after
                .lines()
                .map(str::trim)
                .find(|line| line.starts_with("Detail ")),
            Some("Detail 07"),
            "PgDn advances the detail when the body fits: {after}"
        );
        host.handle_event(&key(KeyCode::Esc));
        assert!(
            task.await.expect("dialog task completes").is_none(),
            "Esc cancels"
        );
    }

    #[tokio::test]
    async fn clicking_detail_activates_only_its_opted_in_choice() {
        for choice in [None, Some(1), Some(99)] {
            let mut dialog = Dialog::new(
                "Sign in",
                "Continue in your browser.",
                choices(&[
                    ("Open browser", "", Some('o')),
                    ("Copy link", "", Some('c')),
                ]),
            )
            .with_detail(
                "Sign-in link",
                format!("https://example.com/{}", "a".repeat(400)),
            );
            if let Some(index) = choice {
                dialog = dialog.with_detail_choice(index);
            }
            let (host, task) = open(dialog).await;
            draw(&host, 80, 24);
            let area = host.state().active.as_ref().unwrap().detail_area.unwrap();
            let click = |kind, row, modifiers| {
                TuiEvent::Mouse(MouseEvent {
                    kind,
                    column: area.x,
                    row,
                    modifiers,
                })
            };
            for event in [
                click(
                    MouseEventKind::Down(MouseButton::Left),
                    area.y - 1,
                    KeyModifiers::NONE,
                ),
                click(
                    MouseEventKind::Down(MouseButton::Right),
                    area.y,
                    KeyModifiers::NONE,
                ),
                click(
                    MouseEventKind::Down(MouseButton::Left),
                    area.y,
                    KeyModifiers::SHIFT,
                ),
                click(
                    MouseEventKind::Down(MouseButton::Left),
                    area.bottom(),
                    KeyModifiers::NONE,
                ),
            ] {
                host.handle_event(&event);
                assert!(
                    host.is_active(),
                    "other mouse actions do not activate the detail"
                );
            }
            host.handle_event(&click(
                MouseEventKind::Down(MouseButton::Left),
                area.y + 1,
                KeyModifiers::NONE,
            ));
            if choice == Some(1) {
                assert!(matches!(task.await.unwrap(), Some(DialogAnswer::Choice(1))));
            } else {
                assert!(
                    host.is_active(),
                    "unset and invalid detail choices do not answer"
                );
                host.handle_event(&key(KeyCode::Esc));
                assert!(task.await.unwrap().is_none());
            }
        }
    }

    #[tokio::test]
    async fn detail_stays_below_choices_and_copies_exactly() {
        let link = format!(
            "https://example.com/authorize?client_id=example&redirect_uri=http%3A%2F%2F127.0.0.1%3A45061%2Fcallback&state={}",
            "a".repeat(160)
        );
        let (host, task) = open(
            Dialog::new(
                "Continue in your browser",
                "Waiting for authorization.",
                choices(&[
                    ("Open browser", "Continue there", Some('o')),
                    ("Copy link", "Copy the complete URL", Some('c')),
                    ("Go back", "Choose another option", None),
                ]),
            )
            .with_detail("Link", link.clone()),
        )
        .await;
        for (width, height) in [(80, 24), (40, 12)] {
            let rows = screen(&host, width, height);
            let text = rows.join("\n");
            for label in ["1. Open browser", "2. Copy link", "3. Go back"] {
                assert!(
                    text.contains(label),
                    "{label} is visible at {width}x{height}: {text}"
                );
            }
            let last_choice = rows.iter().position(|row| row.contains("3. Go back"));
            let first_link = rows.iter().position(|row| row.contains("https://"));
            if let (Some(choice), Some(link)) = (last_choice, first_link) {
                assert!(
                    choice < link,
                    "choices stay above the link at {width}x{height}"
                );
            }
        }
        for width in [37u16, 80, 180] {
            let joined: String = screen(&host, width, 40)
                .iter()
                .map(|row| row.trim_end())
                .collect();
            let rows = screen(&host, width, 40);
            let url = rows.iter().find(|row| row.contains("https://")).unwrap();
            assert_eq!(
                url.find('h'),
                Some(0),
                "link uses the full terminal width at width {width}"
            );
            let terminal = draw(&host, width, 40);
            let row = rows
                .iter()
                .position(|row| row.contains("https://"))
                .unwrap();
            assert_eq!(
                terminal.backend().buffer()[(0, row as u16)].fg,
                Color::DarkGray,
                "link is grey"
            );
            assert!(joined.contains(&link), "link is exact at width {width}");
            assert!(
                !joined.chars().any(|c| "│┌┐└┘─".contains(c)),
                "no borders surround the link at width {width}"
            );
        }
        host.handle_event(&key(KeyCode::Esc));
        assert!(
            task.await.expect("dialog task completes").is_none(),
            "Esc cancels"
        );
    }

    #[tokio::test]
    async fn secret_input_never_renders_and_cancel_discards_it() {
        let (host, task) = open(Dialog::new(
            "Server key",
            "Storage details. ".repeat(40),
            input("do-not-expose", true, Some("Enter a valid key.")),
        ))
        .await;
        host.handle_event(&TuiEvent::Paste("-pasted\n".into()));
        for width in [0, 1, 8, 24, 40, 80] {
            for height in [0, 1, 3, 8, 12, 24, 40] {
                let text = screen(&host, width, height).join("\n");
                assert!(
                    !text.contains("expose") && !text.contains("pasted"),
                    "secret is masked at {width}x{height}"
                );
                if width >= 24 && height >= 12 {
                    assert!(
                        text.contains("Enter a valid key."),
                        "error is visible at {width}x{height}: {text}"
                    );
                    assert!(
                        text.contains('•'),
                        "mask is visible at {width}x{height}: {text}"
                    );
                }
            }
        }
        host.handle_event(&TuiEvent::Key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )));
        assert!(host.is_active(), "a repeated Enter does not submit");
        host.handle_event(&key(KeyCode::Esc));
        assert!(
            task.await.expect("dialog task completes").is_none(),
            "Esc cancels without an answer"
        );
        assert!(!host.is_active(), "cancelling closes the dialog");
    }

    #[tokio::test]
    async fn header_and_detail_adapt_to_available_space() {
        let (host, task) = open(
            Dialog::new("Export", "", input("", false, None))
                .with_header([
                    Text::from("W".repeat(100)),
                    Text::from("M".repeat(50)),
                    Text::styled("Small header", Style::default().fg(Color::Green)),
                ])
                .with_detail("Example command", "export --format json"),
        )
        .await;
        let text = screen(&host, 30, 24).join("\n");
        for shown in ["Small header", "Example command", "export --format json"] {
            assert!(text.contains(shown), "{shown} is visible: {text}");
        }
        assert!(
            draw(&host, 30, 24)
                .backend()
                .buffer()
                .content()
                .iter()
                .any(|cell| cell.symbol() == "S" && cell.fg == Color::Green),
            "header keeps its style"
        );
        let small = screen(&host, 30, 6).join("\n");
        assert!(
            !small.contains("Small header"),
            "header yields space: {small}"
        );
        assert!(small.contains("Export"), "title stays visible: {small}");
        host.handle_event(&TuiEvent::Paste("pc".into()));
        host.handle_event(&key(KeyCode::Enter));
        assert!(
            matches!(task.await.expect("dialog task completes"), Some(DialogAnswer::Input(value)) if value == "pc"),
            "Enter submits the field"
        );
    }

    #[tokio::test]
    async fn workflow_holds_the_last_page_until_replacement_then_releases_it() {
        let host = DialogHost::default();
        let (next, ready) = oneshot::channel();
        let task = tokio::spawn({
            let host = host.clone();
            async move {
                let _workflow = host.begin_workflow();
                let first = host
                    .present(Dialog::new(
                        "First page",
                        "",
                        choices(&[("Next", "", None)]),
                    ))
                    .await;
                assert!(matches!(first, Some(DialogAnswer::Choice(0))));
                ready.await.unwrap();
                host.present(Dialog::new(
                    "Second page",
                    "",
                    choices(&[("Finish", "", None)]),
                ))
                .await
            }
        });
        wait_for(|| host.state().active.is_some()).await;
        assert!(screen(&host, 80, 24).join("\n").contains("First page"));
        host.handle_event(&key(KeyCode::Enter));
        wait_for(|| host.state().retained.is_some()).await;
        assert!(
            host.is_active(),
            "the workflow stays modal between requests"
        );
        assert!(screen(&host, 80, 24).join("\n").contains("First page"));
        assert!(
            host.handle_event(&TuiEvent::Paste("not a session message".into())),
            "gap input is consumed"
        );
        next.send(()).unwrap();
        wait_for(|| host.state().active.is_some()).await;
        let screen = screen(&host, 80, 24).join("\n");
        assert!(screen.contains("Second page"));
        assert!(!screen.contains("First page"));
        host.handle_event(&key(KeyCode::Esc));
        assert!(task.await.unwrap().is_none());
        assert!(
            !host.is_active(),
            "finishing releases the modal immediately"
        );
    }

    #[tokio::test]
    async fn cancelling_a_workflow_releases_its_retained_page() {
        let host = DialogHost::default();
        let task = tokio::spawn({
            let host = host.clone();
            async move {
                let _workflow = host.begin_workflow();
                host.present(Dialog::new(
                    "Transient page",
                    "",
                    choices(&[("Next", "", None)]),
                ))
                .await;
                std::future::pending::<()>().await;
            }
        });
        wait_for(|| host.state().active.is_some()).await;
        host.handle_event(&key(KeyCode::Esc));
        wait_for(|| host.state().retained.is_some()).await;
        assert!(host.is_active());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            !host.is_active(),
            "aborted workflow drops its retained page"
        );
    }

    #[tokio::test]
    async fn present_drop_dismisses_dialog() {
        let (host, task) = open(Dialog::new("Wait", "", DialogContent::Choices(vec![]))).await;
        task.abort();
        let _ = task.await;
        assert!(!host.is_active(), "dropping the request closes the dialog");
    }
}
