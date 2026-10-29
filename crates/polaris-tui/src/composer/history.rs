//! Host-supplied prompt persistence and the composer's bounded history cache.

use super::Composer;
use std::io;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Explicit history identity supplied by the host; never inferred from cwd.
///
/// `project` should identify a working directory stably. `session` identifies
/// the current session. The store decides whether project history spans
/// sessions, and must never return another project's prompts.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PromptHistoryNamespace {
    /// Host's stable project identity (for example a canonical directory).
    pub project: String,
    /// Host's session identity.
    pub session: String,
}

/// Host-owned persistence for accepted prompts.
///
/// Methods run serially on blocking workers, never on the render loop. The
/// host chooses storage location, retention and cross-session aggregation;
/// the TUI caches at most the configured limit. No store is installed by
/// default. Approval replies and refused submissions never call `append`.
///
/// Errors are displayed without undoing a successful submission. The prompt
/// remains in the current panel's cache; later operations can recover, but a
/// failed write is not automatically replayed (an error may follow a write).
/// Implementations shared by panels must coordinate their own concurrent IO.
pub trait PromptHistoryStore: Send + Sync + 'static {
    /// Load at most `limit` recent prompts, oldest first, for this namespace.
    fn load(&self, namespace: &PromptHistoryNamespace, limit: usize) -> io::Result<Vec<String>>;

    /// Persist exactly one accepted prompt, preserving Unicode and newlines.
    /// Enforce `limit` as the host's retained prompt count for the project.
    fn append(
        &self,
        namespace: &PromptHistoryNamespace,
        prompt: &str,
        limit: usize,
    ) -> io::Result<()>;
}

/// Opt-in history configuration for a conversation panel or its plugin.
///
/// # Example
/// ```
/// use std::{num::NonZeroUsize, sync::Arc};
/// use polaris_tui::{PromptHistoryConfig, PromptHistoryNamespace, PromptHistoryStore};
/// use polaris_tui::panels::ConversationPanelPlugin;
///
/// fn conversation(store: Arc<dyn PromptHistoryStore>) -> ConversationPanelPlugin {
///     ConversationPanelPlugin::default().with_history(PromptHistoryConfig::new(
///         store,
///         PromptHistoryNamespace { project: "project-a".into(), session: "session-1".into() },
///         NonZeroUsize::new(500).unwrap(),
///     ))
/// }
/// ```
#[derive(Clone)]
pub struct PromptHistoryConfig {
    pub(crate) namespace: PromptHistoryNamespace,
    store: Arc<dyn PromptHistoryStore>,
    limit: usize,
}

impl PromptHistoryConfig {
    /// Configure a host store, an explicit project/session identity and a cap.
    #[must_use]
    pub fn new(
        store: Arc<dyn PromptHistoryStore>,
        namespace: PromptHistoryNamespace,
        limit: NonZeroUsize,
    ) -> Self {
        Self {
            store,
            namespace,
            limit: limit.get(),
        }
    }
}

enum Update {
    Loaded(io::Result<Vec<String>>),
    Saved(io::Result<()>),
}

struct Search {
    original: Composer,
    query: Composer,
    found: Option<usize>,
}

pub(crate) struct History {
    entries: Vec<String>,
    limit: usize,
    requests: mpsc::Sender<String>,
    updates: mpsc::UnboundedReceiver<Update>,
    loaded: bool,
    notice: Option<String>,
    draft: Option<Composer>,
    index: Option<usize>,
    search: Option<Search>,
}

impl History {
    pub(crate) fn new(config: PromptHistoryConfig) -> Self {
        let (requests, mut pending) = mpsc::channel::<String>(64);
        let (updates_tx, updates) = mpsc::unbounded_channel();
        let limit = config.limit;
        tokio::spawn(async move {
            let load = config.clone();
            let result =
                tokio::task::spawn_blocking(move || load.store.load(&load.namespace, load.limit))
                    .await
                    .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
            let _ = updates_tx.send(Update::Loaded(result));
            while let Some(prompt) = pending.recv().await {
                let save = config.clone();
                let result = tokio::task::spawn_blocking(move || {
                    save.store.append(&save.namespace, &prompt, save.limit)
                })
                .await
                .unwrap_or_else(|error| Err(io::Error::other(error.to_string())));
                // Drain already accepted writes even after the panel closes.
                let _ = updates_tx.send(Update::Saved(result));
            }
        });
        Self {
            entries: Vec::new(),
            limit,
            requests,
            updates,
            loaded: false,
            notice: None,
            draft: None,
            index: None,
            search: None,
        }
    }

    pub(crate) fn poll(&mut self) {
        while let Ok(update) = self.updates.try_recv() {
            match update {
                Update::Loaded(result) => {
                    self.loaded = true;
                    match result {
                        Ok(mut entries) => {
                            // Prompts accepted while loading follow the stored
                            // snapshot. The actor loads before doing any writes.
                            entries.append(&mut self.entries);
                            self.entries = entries;
                            self.cap();
                        }
                        Err(error) => {
                            self.notice = Some(format!(
                                "History unavailable: {error}; using this session's cache"
                            ))
                        }
                    }
                }
                Update::Saved(Ok(())) => {}
                Update::Saved(Err(error)) => {
                    self.notice = Some(format!(
                        "History not saved: {error}; prompt is queued and cached for this session"
                    ))
                }
            }
        }
    }

    fn cap(&mut self) {
        let excess = self.entries.len().saturating_sub(self.limit);
        self.entries.drain(..excess);
    }

    pub(crate) fn record(&mut self, prompt: &str) {
        self.entries.push(prompt.to_owned());
        self.cap();
        self.reset();
        if self.requests.try_send(prompt.to_owned()).is_err() {
            self.notice = Some(
                "History storage is busy or stopped; prompt is queued and cached for this session"
                    .into(),
            );
        }
    }

    pub(crate) fn reset(&mut self) {
        self.draft = None;
        self.index = None;
        self.search = None;
    }

    pub(crate) fn previous(&mut self, composer: &mut Composer) -> bool {
        if !self.loaded || self.entries.is_empty() {
            return false;
        }
        if self.draft.is_none() {
            self.draft = Some(composer.clone());
        }
        let index = self.index.unwrap_or(self.entries.len()).saturating_sub(1);
        self.index = Some(index);
        replace(composer, &self.entries[index]);
        true
    }

    pub(crate) fn next(&mut self, composer: &mut Composer) -> bool {
        let Some(index) = self.index else {
            return false;
        };
        if index + 1 < self.entries.len() {
            self.index = Some(index + 1);
            replace(composer, &self.entries[index + 1]);
        } else {
            self.cancel(composer);
        }
        true
    }

    pub(crate) fn cancel(&mut self, composer: &mut Composer) -> bool {
        if let Some(search) = self.search.take() {
            *composer = search.original;
            return true;
        }
        if let Some(draft) = self.draft.take() {
            *composer = draft;
            self.index = None;
            return true;
        }
        false
    }

    pub(crate) fn searching(&self) -> bool {
        self.search.is_some()
    }

    pub(crate) fn search(&mut self, composer: &mut Composer) {
        if !self.loaded {
            return;
        }
        if self.search.is_some() {
            let before = self
                .search
                .as_ref()
                .and_then(|search| search.found)
                .unwrap_or(0);
            self.find(composer, before);
        } else {
            self.search = Some(Search {
                original: composer.clone(),
                query: Composer::new(),
                found: None,
            });
            self.find(composer, self.entries.len());
        }
    }

    pub(crate) fn query_insert(&mut self, composer: &mut Composer, text: &str) {
        if let Some(search) = &mut self.search {
            search.query.insert_paste(text);
        }
        self.find(composer, self.entries.len());
    }

    pub(crate) fn query_backspace(&mut self, composer: &mut Composer) {
        if let Some(search) = &mut self.search {
            search.query.backspace();
        }
        self.find(composer, self.entries.len());
    }

    fn find(&mut self, composer: &mut Composer, before: usize) {
        let Some(search) = &mut self.search else {
            return;
        };
        search.found = self.entries[..before]
            .iter()
            .rposition(|entry| entry.contains(search.query.text()));
        if let Some(index) = search.found {
            replace(composer, &self.entries[index]);
        }
    }

    pub(crate) fn accept_search(&mut self, composer: &mut Composer) {
        if let Some(search) = self.search.take() {
            if search.found.is_none() {
                *composer = search.original;
            }
            self.draft = None;
            self.index = None;
        }
    }

    pub(crate) fn banner(&self) -> Option<String> {
        if let Some(search) = &self.search {
            let result = if search.found.is_some() {
                "match"
            } else {
                "no match"
            };
            return Some(format!(
                "Search: {} — {result} · Ctrl-R older · Enter accept · Esc cancel",
                search.query.text().replace('\n', " ")
            ));
        }
        if !self.loaded {
            return Some("Loading prompt history…".into());
        }
        self.notice.clone()
    }
}

pub(crate) fn replace(composer: &mut Composer, text: &str) {
    composer.clear();
    composer.insert_paste(text);
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[derive(Default)]
    pub(crate) struct Store {
        pub(crate) prompts: Mutex<HashMap<String, Vec<String>>>,
        pub(crate) namespaces: Mutex<Vec<PromptHistoryNamespace>>,
        pub(crate) fail_load: AtomicBool,
        pub(crate) fail_write: AtomicBool,
        pub(crate) writes: AtomicUsize,
    }

    impl PromptHistoryStore for Store {
        fn load(&self, namespace: &PromptHistoryNamespace, _: usize) -> io::Result<Vec<String>> {
            if self.fail_load.load(Ordering::SeqCst) {
                return Err(io::Error::other("storage offline"));
            }
            Ok(self
                .prompts
                .lock()
                .get(&namespace.project)
                .cloned()
                .unwrap_or_default())
        }
        fn append(
            &self,
            namespace: &PromptHistoryNamespace,
            prompt: &str,
            limit: usize,
        ) -> io::Result<()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(io::Error::other("disk full"));
            }
            self.namespaces.lock().push(namespace.clone());
            let mut all = self.prompts.lock();
            let entries = all.entry(namespace.project.clone()).or_default();
            entries.push(prompt.into());
            entries.drain(..entries.len().saturating_sub(limit));
            Ok(())
        }
    }

    pub(crate) fn config(
        store: Arc<Store>,
        project: &str,
        session: &str,
        limit: usize,
    ) -> PromptHistoryConfig {
        PromptHistoryConfig::new(
            store,
            PromptHistoryNamespace {
                project: project.into(),
                session: session.into(),
            },
            NonZeroUsize::new(limit).unwrap(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    async fn ready(history: &mut History) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                history.poll();
                if history.loaded {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn draft(text: &str) -> Composer {
        let mut composer = Composer::new();
        replace(&mut composer, text);
        composer
    }

    #[tokio::test]
    async fn history_is_capped_and_cancel_restores_text_and_caret() {
        let store = Arc::new(Store::default());
        store.prompts.lock().insert(
            "project".into(),
            vec!["dropped".into(), "older".into(), "newest".into()],
        );
        let mut history = History::new(config(store, "project", "session", 2));
        ready(&mut history).await;
        let mut composer = draft("世\noriginal");
        composer.seek(3);
        assert!(history.previous(&mut composer));
        assert_eq!(composer.text(), "newest");
        assert!(history.previous(&mut composer));
        assert_eq!(composer.text(), "older");
        assert!(history.previous(&mut composer));
        assert_eq!(
            composer.text(),
            "older",
            "the capped entry cannot be recalled"
        );
        assert!(history.next(&mut composer));
        assert_eq!(composer.text(), "newest");
        assert!(history.next(&mut composer));
        assert_eq!(composer.text(), "世\noriginal");
        assert_eq!(composer.cursor(), 3);
        assert!(!history.next(&mut composer));
    }

    #[tokio::test]
    async fn reverse_search_incremental_accept_cancel_and_no_match() {
        let store = Arc::new(Store::default());
        store.prompts.lock().insert(
            "p".into(),
            vec![
                "古い世界".into(),
                "unrelated".into(),
                "new 世界\nline".into(),
            ],
        );
        let mut history = History::new(config(store, "p", "s", 10));
        ready(&mut history).await;
        let mut composer = draft("draft");
        composer.seek(2);
        history.search(&mut composer);
        history.query_insert(&mut composer, "世界");
        assert_eq!(composer.text(), "new 世界\nline");
        history.search(&mut composer);
        assert_eq!(composer.text(), "古い世界");
        history.cancel(&mut composer);
        assert_eq!(composer.text(), "draft");
        assert_eq!(composer.cursor(), 2);
        history.search(&mut composer);
        history.query_insert(&mut composer, "世界x");
        assert!(history.banner().unwrap().contains("no match"));
        history.query_backspace(&mut composer);
        assert_eq!(composer.text(), "new 世界\nline");
        history.accept_search(&mut composer);
        assert!(!history.searching());
        assert_eq!(composer.text(), "new 世界\nline");
        history.search(&mut composer);
        history.query_insert(&mut composer, "absent");
        history.accept_search(&mut composer);
        assert_eq!(
            composer.text(),
            "new 世界\nline",
            "accept with no match restores the original draft"
        );
    }

    #[tokio::test]
    async fn offline_store_keeps_session_cache_and_later_writes_can_recover() {
        let store = Arc::new(Store::default());
        store.fail_load.store(true, Ordering::SeqCst);
        store.fail_write.store(true, Ordering::SeqCst);
        let mut history = History::new(config(store.clone(), "p", "session-id", 10));
        ready(&mut history).await;
        assert!(history.banner().unwrap().contains("storage offline"));
        history.record("not saved");
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                history.poll();
                if history.banner().unwrap().contains("disk full") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let mut composer = draft("");
        history.previous(&mut composer);
        assert_eq!(composer.text(), "not saved");
        store.fail_write.store(false, Ordering::SeqCst);
        history.record("saved 世界\nline");
        drop(history);
        tokio::time::timeout(Duration::from_secs(2), async {
            while store.namespaces.lock().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.prompts.lock()["p"], ["saved 世界\nline"]);
        assert_eq!(store.namespaces.lock()[0].session, "session-id");
    }
}
