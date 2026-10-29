//! Conversation browsing, read-only host history, and explicit plain-text export.
use super::*;
use std::future::Future;
use std::pin::Pin;

/// One bounded, chronological page from the host's canonical conversation.
/// Replaces the previous history page; it is never merged into live IO.
#[derive(Clone, Debug)]
pub struct HistoryPage {
    /// Index of the first entry in this page.
    pub start: usize,
    /// Total entries available at the time of the read.
    pub total: usize,
    /// At most the requested limit, in chronological order.
    pub messages: VecDeque<IOMessage>,
}

/// Result of a non-blocking request for canonical history.
pub type HistoryFuture = Pin<Box<dyn Future<Output = Result<HistoryPage, String>> + Send>>;

/// Host-owned, read-only history. Implementations must never execute a turn.
///
/// `before` is an exclusive entry index; `None` requests the newest page.
/// Return at most `limit` entries immediately preceding that index. Entries
/// must have stable chronological indices within the active session. A session
/// switch must create a fresh panel/source, not reuse another session's cursor.
/// The panel cancels abandoned requests and keeps live IO separate from replay.
pub trait TranscriptHistory: Send + Sync + 'static {
    /// Read a page without blocking the terminal event loop.
    fn load(&self, before: Option<usize>, limit: usize) -> HistoryFuture;
}

#[derive(Default)]
pub(super) struct BrowseState {
    pub active: bool,
    pub starts: Vec<(usize, usize)>,
    pub selected: Option<usize>,
    pub code: Option<usize>,
    pub query: String,
    pub editing_search: bool,
    pub found: Option<(usize, usize)>,
    pub notice: String,
    pub page: Option<HistoryPage>,
    pub history_revision: u64,
    pub source: Option<Arc<dyn TranscriptHistory>>,
    pub export: Option<(Composer, String)>,
    history_job: Option<tokio::task::JoinHandle<()>>,
    history_rx: Option<tokio::sync::oneshot::Receiver<Result<HistoryPage, String>>>,
    export_rx: Option<tokio::sync::oneshot::Receiver<Result<String, String>>>,
}

impl Drop for BrowseState {
    fn drop(&mut self) {
        if let Some(job) = self.history_job.take() {
            job.abort();
        }
    }
}

impl ConversationPanel {
    /// Supply read-only canonical history for Ctrl-G, H browsing.
    /// Plain consumers may omit this; the retained live view remains usable.
    #[must_use]
    pub fn with_history_source(mut self, source: Arc<dyn TranscriptHistory>) -> Self {
        self.browse.source = Some(source);
        self
    }

    pub(super) fn browse_messages(&self) -> &VecDeque<IOMessage> {
        self.browse
            .page
            .as_ref()
            .map_or(&self.transcript, |p| &p.messages)
    }

    pub(super) fn browse_banner(&self) -> Option<Line<'static>> {
        if !self.browse.active && self.browse.notice.is_empty() && self.browse.page.is_none() {
            return None;
        }
        let text = if let Some((path, _)) = &self.browse.export {
            format!(
                "Export to new file: {} · Enter saves · Esc cancels",
                safe_text(path.text())
            )
        } else if self.browse.editing_search {
            format!(
                "Find: {} · {} · Enter next · Esc closes",
                safe_text(&self.browse.query),
                self.browse.notice
            )
        } else {
            let origin = self.browse.page.as_ref().map_or_else(
                || format!("live: last {TRANSCRIPT_CAP} entries"),
                |p| {
                    format!(
                        "history {}–{} / {} (completed turns)",
                        p.start + usize::from(!p.messages.is_empty()),
                        p.start + p.messages.len(),
                        p.total
                    )
                },
            );
            format!(
                "{origin} · {} · ↑↓ select · Shift-↑↓ turns · / find · n/N next/prev · c code · e export · a export page · h/l history · End live · Esc close",
                self.browse.notice
            )
        };
        Some(Line::styled(text, Style::default().fg(Color::Cyan)))
    }

    pub(super) fn drain_browse_results(&mut self) {
        if let Some(rx) = &mut self.browse.history_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.browse.history_rx = None;
                    self.browse.history_job = None;
                    match result {
                        Ok(page)
                            if page.messages.len() <= TRANSCRIPT_CAP
                                && page.start <= page.total
                                && page.messages.len() <= page.total - page.start =>
                        {
                            self.browse.page = Some(page);
                            self.browse.history_revision =
                                self.browse.history_revision.wrapping_add(1);
                            self.browse.selected = None;
                            self.browse.code = None;
                            self.browse.found = None;
                            self.browse.notice = "Read-only snapshot; live output continues".into();
                            self.content_rev = self.content_rev.wrapping_add(1);
                            self.view = ScrollableTranscript::new();
                            self.view.apply(NavIntent::Top);
                        }
                        Ok(_) => {
                            self.browse.notice = "Host returned an invalid history page".into()
                        }
                        Err(err) => {
                            self.browse.notice = format!("History unavailable: {}", safe_text(&err))
                        }
                    }
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.browse.history_rx = None;
                    self.browse.notice = "History request ended without a result; H retries".into();
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
        if let Some(rx) = &mut self.browse.export_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.browse.export_rx = None;
                    self.browse.notice =
                        result.unwrap_or_else(|err| format!("Export failed: {}", safe_text(&err)));
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.browse.export_rx = None;
                    self.browse.notice = "Export worker ended without a result".into();
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            }
        }
    }

    fn load_history(&mut self, newer: bool) {
        let Some(source) = self.browse.source.clone() else {
            self.browse.notice = "This host has no canonical history source".into();
            return;
        };
        let before = self.browse.page.as_ref().map(|p| {
            if newer {
                (p.start + p.messages.len())
                    .saturating_add(TRANSCRIPT_CAP)
                    .min(p.total)
            } else {
                p.start
            }
        });
        if before == Some(0) {
            self.browse.notice = "Beginning of canonical history".into();
            return;
        }
        if let Some(job) = self.browse.history_job.take() {
            job.abort();
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.browse.history_rx = Some(rx);
        self.browse.notice = "Loading canonical history…".into();
        self.browse.history_job = Some(tokio::spawn(async move {
            let _ = tx.send(source.load(before, TRANSCRIPT_CAP).await);
        }));
    }

    fn return_live(&mut self) {
        if let Some(job) = self.browse.history_job.take() {
            job.abort();
        }
        self.browse.history_rx = None;
        self.browse.page = None;
        self.browse.selected = None;
        self.browse.code = None;
        self.browse.found = None;
        self.browse.editing_search = false;
        self.browse.export = None;
        self.browse.notice.clear();
        self.browse.active = false;
        self.content_rev = self.content_rev.wrapping_add(1);
        self.view.apply(NavIntent::Bottom);
    }

    fn select_message(&mut self, backwards: bool, turns: bool) {
        let messages = self.browse_messages();
        let candidates: Vec<_> = self
            .browse
            .starts
            .iter()
            .filter(|(i, _)| {
                !turns
                    || matches!(messages[*i].source, IOSource::User)
                        && messages[*i]
                            .metadata
                            .get(IO_TYPE)
                            .is_none_or(|k| k == TYPE_MESSAGE)
            })
            .copied()
            .collect();
        let current = self.browse.selected;
        let next = if backwards {
            candidates
                .iter()
                .rev()
                .find(|(i, _)| current.is_none_or(|c| *i < c))
        } else {
            candidates
                .iter()
                .find(|(i, _)| current.is_none_or(|c| *i > c))
        };
        if let Some(&(index, line)) = next {
            self.browse.selected = Some(index);
            self.browse.code = None;
            self.view.jump_to_line(line);
            self.browse.notice = format!("Selected message {}", index + 1);
        }
    }

    fn search_browse(&mut self, advance: bool, backwards: bool) {
        self.browse.found = self.view.find_occurrence(
            &self.browse.query,
            if advance { self.browse.found } else { None },
            backwards,
        );
        if let Some((line, occurrence)) = self.browse.found {
            self.view
                .jump_to_occurrence(line, &self.browse.query, occurrence);
            self.browse.selected = self
                .browse
                .starts
                .iter()
                .rev()
                .find(|(_, start)| *start <= line)
                .map(|(i, _)| *i);
            self.browse.code = None;
            self.browse.notice = format!(
                "Match in message {}",
                self.browse.selected.map_or(0, |i| i + 1)
            );
        } else {
            self.browse.notice = if self.browse.query.is_empty() {
                "Type to search"
            } else {
                "No match"
            }
            .into();
        }
    }

    fn start_export(&mut self, all: bool) {
        if self.browse.export_rx.is_some() {
            self.browse.notice = "Export still running".into();
            return;
        }
        let messages = self.browse_messages();
        let text = if all {
            messages
                .iter()
                .filter(|m| exportable(m))
                .map(|m| other_body(&m.content))
                .collect::<Vec<_>>()
                .join("\n\n")
        } else if let Some(message) = self.browse.selected.and_then(|i| messages.get(i)) {
            if !exportable(message) {
                self.browse.notice = "Control metadata is not exportable".into();
                return;
            }
            let body = other_body(&message.content);
            match self.browse.code {
                Some(i) => code_blocks(&body).get(i).cloned().unwrap_or_default(),
                None => body,
            }
        } else {
            self.browse.notice = "Select a message with ↑ or ↓ first".into();
            return;
        };
        self.browse.export = Some((Composer::new(), text));
    }

    pub(super) fn browse_paste(&mut self, text: &str) -> bool {
        if let Some((path, _)) = &mut self.browse.export {
            path.insert_paste(text);
            return true;
        }
        if self.browse.editing_search {
            self.browse
                .query
                .push_str(&safe_text(text).replace('\n', " "));
            self.search_browse(false, false);
            return true;
        }
        self.browse.active
    }

    pub(super) fn browse_key(&mut self, key: KeyEvent) -> Option<EventOutcome> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::End && ctrl {
            self.return_live();
            return Some(EventOutcome::Consumed);
        }
        if key.code == KeyCode::Char('g') && ctrl {
            self.browse.active = true;
            self.browse.notice = "Browse transcript".into();
            self.close_popup();
            return Some(EventOutcome::Consumed);
        }
        if !self.browse.active {
            return None;
        }
        // Preserve approval and interruption priority; browsing never answers
        // an approval or submits its search/path as a prompt.
        if key.code == KeyCode::Char('c') && ctrl {
            return None;
        }
        if let Some((path, _)) = &mut self.browse.export {
            match key.code {
                KeyCode::Esc => self.browse.export = None,
                KeyCode::Enter => {
                    let (path, text) = self.browse.export.take().unwrap();
                    if path.text().trim().is_empty() {
                        self.browse.notice = "Export cancelled: empty path".into();
                    } else {
                        let path = path.text().to_owned();
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        self.browse.export_rx = Some(rx);
                        self.browse.notice = "Saving export…".into();
                        tokio::task::spawn_blocking(move || {
                            let _ = tx.send(write_export(&path, &text));
                        });
                    }
                }
                KeyCode::Backspace => {
                    path.backspace();
                }
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    path.insert_char(c)
                }
                _ => {}
            }
        } else if self.browse.editing_search {
            match key.code {
                KeyCode::Esc => self.browse.editing_search = false,
                KeyCode::Enter => {
                    self.search_browse(true, key.modifiers.contains(KeyModifiers::SHIFT))
                }
                KeyCode::Backspace => {
                    self.browse.query.pop();
                    self.search_browse(false, false);
                }
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                    self.browse.query.push(c);
                    self.search_browse(false, false);
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc => {
                    self.browse.active = false;
                    self.browse.notice.clear();
                }
                KeyCode::End => self.return_live(),
                KeyCode::Home => self.view.apply(NavIntent::Top),
                KeyCode::PageUp => self.view.apply(NavIntent::PageUp),
                KeyCode::PageDown => self.view.apply(NavIntent::PageDown),
                KeyCode::Up => {
                    self.select_message(true, key.modifiers.contains(KeyModifiers::SHIFT))
                }
                KeyCode::Down => {
                    self.select_message(false, key.modifiers.contains(KeyModifiers::SHIFT))
                }
                KeyCode::Char('/') => {
                    self.browse.editing_search = true;
                    self.browse.query.clear();
                    self.search_browse(false, false);
                }
                KeyCode::Char('n') => self.search_browse(true, false),
                KeyCode::Char('N') => self.search_browse(true, true),
                KeyCode::Char('h') => self.load_history(false),
                KeyCode::Char('l') => self.load_history(true),
                KeyCode::Char('e') => self.start_export(false),
                KeyCode::Char('a') => self.start_export(true),
                KeyCode::Char('c') => {
                    let messages = self.browse_messages();
                    let count = self
                        .browse
                        .selected
                        .and_then(|i| messages.get(i))
                        .map_or(0, |m| code_blocks(&other_body(&m.content)).len());
                    if count > 0 {
                        self.browse.code = Some(self.browse.code.map_or(0, |i| (i + 1) % count));
                        self.browse.notice = format!(
                            "Code block {} / {count} selected · e exports",
                            self.browse.code.unwrap() + 1
                        );
                    } else {
                        self.browse.notice = "Selected message has no fenced code".into();
                    }
                }
                _ => {}
            }
        }
        Some(EventOutcome::Consumed)
    }
}

fn exportable(message: &IOMessage) -> bool {
    !unknown_message(message)
        && !matches!(
            message.metadata.get(IO_TYPE).map(String::as_str),
            Some(TYPE_STATUS | TYPE_TOKENS)
        )
}

fn code_blocks(body: &str) -> Vec<String> {
    use pulldown_cmark::{Event, Parser, Tag, TagEnd};
    let mut blocks = Vec::new();
    let mut current = None;
    for event in Parser::new(body) {
        match event {
            Event::Start(Tag::CodeBlock(_)) => current = Some(String::new()),
            Event::Text(text) if current.is_some() => current.as_mut().unwrap().push_str(&text),
            Event::End(TagEnd::CodeBlock) => {
                if let Some(text) = current.take() {
                    blocks.push(text);
                }
            }
            _ => {}
        }
    }
    blocks
}

fn write_export(path: &str, text: &str) -> Result<String, String> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| err.to_string())?;
    file.write_all(text.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|err| format!("{err}; a partial export may remain at {}", safe_text(path)))?;
    Ok(format!(
        "Exported {} bytes to {}",
        text.len(),
        safe_text(path)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(panel: &mut ConversationPanel, code: KeyCode, modifiers: KeyModifiers) {
        panel.handle_key(KeyEvent::new(code, modifiers));
    }
    fn draw(panel: &mut ConversationPanel, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| panel.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    fn enter(panel: &mut ConversationPanel) {
        key(panel, KeyCode::Char('g'), KeyModifiers::CONTROL);
    }

    #[tokio::test]
    async fn search_wrap_and_no_match_preserve_draft_and_never_submit() {
        let (_, mut panel) = TuiIOProvider::new();
        panel.composer.insert_paste("unfinished draft");
        for i in 0..30 {
            panel.push_transcript(IOMessage::user_text(format!("turn {i} needle")));
        }
        draw(&mut panel, 50);
        enter(&mut panel);
        key(&mut panel, KeyCode::Char('/'), KeyModifiers::NONE);
        assert!(panel.browse_paste("needle"));
        assert_eq!(panel.browse.selected, Some(0));
        for _ in 0..30 {
            key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
        }
        assert_eq!(panel.browse.selected, Some(0));
        key(&mut panel, KeyCode::Enter, KeyModifiers::SHIFT);
        assert_eq!(panel.browse.selected, Some(29));
        panel.browse_paste("missing");
        assert_eq!(panel.browse.notice, "No match");
        assert_eq!(panel.composer.text(), "unfinished draft");
        assert_eq!(panel.in_tx.capacity(), panel.in_tx.max_capacity());
    }

    #[tokio::test]
    async fn user_turn_jumps_export_fidelity_and_live_tail() {
        let (_, mut panel) = TuiIOProvider::new();
        panel.push_transcript(IOMessage::user_text("first turn"));
        let body = "```rust\n\tlet x = \"世界\";\n```\n\nafter";
        panel.push_transcript(
            IOMessage::from_agent("code", IOContent::Text(body.into()))
                .with_metadata(IO_TYPE, "message"),
        );
        panel.push_transcript(
            IOMessage::system_text("hidden secret").with_metadata(IO_TYPE, "future-control"),
        );
        panel.push_transcript(IOMessage::user_text("second turn"));
        draw(&mut panel, 80);
        enter(&mut panel);
        key(&mut panel, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(panel.browse.selected, Some(0));
        key(&mut panel, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(panel.browse.selected, Some(3));
        key(&mut panel, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(panel.browse.selected, Some(1));
        key(&mut panel, KeyCode::Char('c'), KeyModifiers::NONE);
        key(&mut panel, KeyCode::Char('e'), KeyModifiers::NONE);
        assert_eq!(
            panel.browse.export.as_ref().unwrap().1,
            "\tlet x = \"世界\";\n"
        );
        key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut panel, KeyCode::Char('a'), KeyModifiers::NONE);
        let exported = &panel.browse.export.as_ref().unwrap().1;
        assert!(exported.contains(body));
        assert!(!exported.contains("hidden secret"));
        assert!(!exported.contains("queued"));
        key(&mut panel, KeyCode::End, KeyModifiers::CONTROL);
        assert!(panel.view.is_following());
        assert!(!panel.browse.active);
    }

    #[tokio::test]
    async fn append_and_eviction_keep_the_visible_message_and_tail_following() {
        let (_, mut panel) = TuiIOProvider::new();
        for i in 0..TRANSCRIPT_CAP {
            panel.push_transcript(IOMessage::user_text(format!("message {i:04}")));
        }
        draw(&mut panel, 60);
        panel.push_transcript(IOMessage::user_text("new tail"));
        assert!(
            panel.view.is_following(),
            "cap eviction must not unpin the tail"
        );
        draw(&mut panel, 60);
        enter(&mut panel);
        panel.view.jump_to_line(10);
        let before = draw(&mut panel, 60);
        assert!(before.starts_with("you ❯ message 0011"));
        panel.push_transcript(IOMessage::user_text("another tail"));
        let after = draw(&mut panel, 60);
        assert!(after.starts_with("you ❯ message 0011"));
        assert!(!panel.view.is_following());
    }

    struct History;
    impl TranscriptHistory for History {
        fn load(&self, before: Option<usize>, limit: usize) -> HistoryFuture {
            Box::pin(async move {
                let end = before.unwrap_or(3000).min(3000);
                let start = end.saturating_sub(limit);
                Ok(HistoryPage {
                    start,
                    total: 3000,
                    messages: (start..end)
                        .map(|i| IOMessage::user_text(format!("canonical {i}")))
                        .collect(),
                })
            })
        }
    }

    #[tokio::test]
    async fn history_pages_replace_each_other_live_continues_and_return_cancels_load() {
        let (_, panel) = TuiIOProvider::new();
        let mut panel = panel.with_history_source(Arc::new(History));
        panel.push_transcript(IOMessage::user_text("live only"));
        draw(&mut panel, 100);
        enter(&mut panel);
        panel.load_history(false);
        tokio::task::yield_now().await;
        panel.drain_browse_results();
        assert_eq!(panel.browse.page.as_ref().unwrap().start, 1976);
        panel.load_history(false);
        tokio::task::yield_now().await;
        panel.drain_browse_results();
        let page = panel.browse.page.as_ref().unwrap();
        assert_eq!((page.start, page.messages.len()), (952, 1024));
        assert_eq!(panel.transcript.len(), 1);
        panel.push_transcript(IOMessage::user_text("live while browsing"));
        assert_eq!(panel.browse.page.as_ref().unwrap().messages.len(), 1024);
        panel.load_history(true);
        tokio::task::yield_now().await;
        panel.drain_browse_results();
        assert_eq!(panel.browse.page.as_ref().unwrap().start, 1976);
        panel.load_history(false);
        panel.return_live();
        tokio::task::yield_now().await;
        panel.drain_browse_results();
        assert!(panel.browse.page.is_none());
        assert_eq!(panel.transcript.len(), 2);
        assert!(draw(&mut panel, 100).contains("live while browsing"));
    }

    struct BrokenHistory;
    impl TranscriptHistory for BrokenHistory {
        fn load(&self, _: Option<usize>, _: usize) -> HistoryFuture {
            Box::pin(async { Err("store unavailable".into()) })
        }
    }
    #[tokio::test]
    async fn history_failure_keeps_live_content_and_pending_approval_is_not_answered() {
        let (_, panel) = TuiIOProvider::new();
        let mut panel = panel.with_history_source(Arc::new(BrokenHistory));
        panel.push_transcript(IOMessage::user_text("keep me"));
        panel.pending_confirm = Some(PendingConfirm {
            summary: "run command?".into(),
        });
        enter(&mut panel);
        panel.load_history(false);
        tokio::task::yield_now().await;
        panel.drain_browse_results();
        assert!(panel.browse.notice.contains("store unavailable"));
        assert!(panel.browse.page.is_none());
        key(&mut panel, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(panel.pending_confirm.is_some());
        assert_eq!(panel.in_tx.capacity(), panel.in_tx.max_capacity());
        assert!(draw(&mut panel, 100).contains("keep me"));
    }

    #[test]
    fn export_is_byte_faithful_and_never_overwrites_existing_files() {
        let path = std::env::temp_dir().join(format!(
            "polaris-transcript-export-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = path.to_str().unwrap();
        let content = "\t世界\r\nno trailing newline";
        write_export(path_str, content).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
        assert!(write_export(path_str, "overwrite").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), content.as_bytes());
        std::fs::remove_file(path).unwrap();
    }
}
