//! Consumer proof: the host persists history and controls project isolation.
#![cfg(feature = "panels")]

use polaris::plugins::{IOContent, IOProvider};
use polaris_tui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use polaris_tui::panels::{ConversationPanel, TuiIOProvider};
use polaris_tui::{
    PromptHistoryConfig, PromptHistoryNamespace, PromptHistoryStore, TuiEvent, TuiPanel,
};
use std::fs;
use std::io;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// This fixture deliberately lives in the host, not in the TUI library. Real
// hosts choose their own path, privacy policy, locking and durable write scheme.
struct FileStore {
    path: PathBuf,
    writes: AtomicUsize,
}

impl FileStore {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            writes: AtomicUsize::new(0),
        }
    }

    fn read(&self) -> io::Result<serde_json::Map<String, serde_json::Value>> {
        match fs::read(&self.path) {
            Ok(data) => serde_json::from_slice(&data).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Default::default()),
            Err(error) => Err(error),
        }
    }
}

impl PromptHistoryStore for FileStore {
    fn load(&self, namespace: &PromptHistoryNamespace, limit: usize) -> io::Result<Vec<String>> {
        let mut prompts: Vec<String> = self
            .read()?
            .get(&namespace.project)
            .and_then(|value| value.as_array())
            .into_iter()
            .flatten()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();
        prompts.drain(..prompts.len().saturating_sub(limit));
        Ok(prompts)
    }

    fn append(
        &self,
        namespace: &PromptHistoryNamespace,
        prompt: &str,
        limit: usize,
    ) -> io::Result<()> {
        assert!(
            !namespace.session.is_empty(),
            "host session identity must reach persistence"
        );
        let mut all = self.read()?;
        let prompts = all
            .entry(namespace.project.clone())
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .unwrap();
        prompts.push(serde_json::Value::String(prompt.into()));
        prompts.drain(..prompts.len().saturating_sub(limit));
        fs::write(&self.path, serde_json::to_vec(&all)?)?;
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("history.json"));
        let _ = fs::remove_dir(&self.0);
    }
}

fn panel(
    store: Arc<FileStore>,
    project: &str,
    session: &str,
) -> (TuiIOProvider, ConversationPanel) {
    let (provider, panel) = TuiIOProvider::new();
    (
        provider,
        panel.with_history(PromptHistoryConfig::new(
            store,
            PromptHistoryNamespace {
                project: project.into(),
                session: session.into(),
            },
            NonZeroUsize::new(2).unwrap(),
        )),
    )
}

fn key(panel: &mut ConversationPanel, code: KeyCode) {
    panel.handle_event(&TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

async fn flush(store: &FileStore, count: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while store.writes.load(Ordering::SeqCst) < count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

async fn recall(provider: &TuiIOProvider, panel: &mut ConversationPanel, expected: &str) {
    use polaris_tui::ratatui::{Terminal, backend::TestBackend};
    let mut terminal = Terminal::new(TestBackend::new(100, 15)).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            panel.tick();
            terminal
                .draw(|frame| panel.render(frame, frame.area()))
                .unwrap();
            let screen: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            if !screen.contains("Loading prompt history") {
                break;
            }
            tokio::task::yield_now().await;
        }
        key(panel, KeyCode::Up);
        key(panel, KeyCode::Enter);
        match provider.receive().await.unwrap().content {
            IOContent::Text(text) => assert_eq!(text, expected),
            other => panic!("expected a recalled text prompt, got {other:?}"),
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn restarting_panels_recovers_host_files_without_crossing_projects() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = std::env::temp_dir().join(format!(
        "polaris-history-proof-{}-{unique}",
        std::process::id()
    ));
    fs::create_dir(&directory).unwrap();
    let fixture = Fixture(directory);
    let path = fixture.0.join("history.json");

    let store = Arc::new(FileStore::new(path.clone()));
    let (_provider, mut first) = panel(store.clone(), "/project/one", "session-one");
    first.handle_event(&TuiEvent::Paste("first 世界\nline\n".into()));
    key(&mut first, KeyCode::Enter);
    flush(&store, 1).await;
    drop(first);

    let store = Arc::new(FileStore::new(path.clone()));
    let (_provider, mut second) = panel(store.clone(), "/project/two", "session-two");
    second.handle_event(&TuiEvent::Paste("other project's prompt".into()));
    key(&mut second, KeyCode::Enter);
    flush(&store, 1).await;
    drop(second);

    // New host objects, same file; project identity selects its own history
    // even though the host aggregates that project's history across sessions.
    for (project, expected) in [
        ("/project/one", "first 世界\nline\n"),
        ("/project/two", "other project's prompt"),
    ] {
        let store = Arc::new(FileStore::new(path.clone()));
        let (provider, mut resumed) = panel(store.clone(), project, "new-session");
        recall(&provider, &mut resumed, expected).await;
        flush(&store, 1).await;
    }
}
