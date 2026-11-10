use super::*;
use crate::chatgpt::ChatGptStore;
use crate::credentials::{CredentialStore, StoredCredentials};
use crate::providers::Provider;
use polaris_tui::{
    TuiEvent,
    dialogs::DialogHost,
    ratatui::{
        Terminal,
        backend::TestBackend,
        crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
    },
};

fn draw(host: &DialogHost, width: u16, height: u16) -> String {
    let mut terminal =
        Terminal::new(TestBackend::new(width, height)).expect("test backend creates a terminal");
    terminal
        .draw(|frame| {
            host.render(frame, frame.area());
        })
        .expect("dialog host draws into the test backend");
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(usize::from(width.max(1)))
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

async fn wait_for(host: &DialogHost, heading: &str) -> String {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let text = draw(host, 90, 25);
            if text.lines().any(|line| line.trim() == heading) {
                return text;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("setup reached the expected dialog")
}

fn key(host: &DialogHost, code: KeyCode) {
    host.handle_event(&TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
}

async fn enter(host: &DialogHost, text: &str) {
    host.handle_event(&TuiEvent::Key(KeyEvent::new(
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    )));
    host.handle_event(&TuiEvent::Paste(text.into()));
    // A preceding dialog answer guards confirmation until typing has paused.
    tokio::time::sleep(std::time::Duration::from_millis(450)).await;
    key(host, KeyCode::Enter);
}

fn setup(dir: &tempfile::TempDir, host: DialogHost) -> SetupService {
    setup_with_env(dir, host, |_| None)
}

fn setup_with_env(
    dir: &tempfile::TempDir,
    host: DialogHost,
    env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> SetupService {
    SetupService::new(
        host,
        CredentialStore::at(dir.path().join("credentials.toml")),
        ChatGptStore::at(dir.path().join("chatgpt.json")),
        env,
    )
}

#[tokio::test]
async fn injected_api_key_offers_existing_credentials() {
    for provider in [Provider::OpenAi, Provider::OpenAiChat, Provider::Anthropic] {
        let dir = tempfile::tempdir().expect("temporary directory is created");
        let host = DialogHost::default();
        let variable = provider.key_var().expect("API key provider");
        let service = setup_with_env(&dir, host.clone(), move |name| {
            (name == variable).then(|| "environment-key".into())
        });
        let task = tokio::spawn(async move { service.connect(Some(provider)).await });
        let screen = wait_for(&host, &format!("Connect to {}", provider.name())).await;
        for expected in [
            format!("An API key already exists for {}.", provider.name()),
            "Use existing API key".into(),
            "Enter a different API key".into(),
        ] {
            assert!(screen.contains(&expected), "{provider:?}: {expected}");
        }
        key(&host, KeyCode::Esc);
        wait_for(&host, "Welcome to Polaris Code").await;
        key(&host, KeyCode::Esc);
        assert!(
            task.await
                .expect("setup task completes")
                .expect("cancellation succeeds")
                .is_none(),
            "cancelling does not activate the detected credentials"
        );
    }
}

#[tokio::test]
async fn blank_region_variables_fall_back_to_next_nonblank_value() {
    let dir = tempfile::tempdir().expect("temporary directory is created");
    let host = DialogHost::default();
    let service = setup_with_env(&dir, host.clone(), |name| match name {
        "POLARIS_CODE_REGION" => Some("  ".into()),
        "AWS_REGION" => Some("us-west-2".into()),
        _ => None,
    });
    let task = tokio::spawn(async move { service.connect(Some(Provider::Bedrock)).await });
    let screen = wait_for(&host, "AWS Bedrock region").await;
    assert!(
        screen.contains("us-west-2"),
        "blank higher-priority variables do not hide the usable AWS region"
    );
    key(&host, KeyCode::Esc);
    wait_for(&host, "Welcome to Polaris Code").await;
    key(&host, KeyCode::Esc);
    assert!(
        task.await
            .expect("setup task completes")
            .expect("cancellation succeeds")
            .is_none(),
        "cancelling the region prompt connects nothing"
    );
}

#[tokio::test]
async fn launch_model_keeps_custom_chat_model() {
    let dir = tempfile::tempdir().expect("temporary directory is created");
    let credentials = CredentialStore::at(dir.path().join("credentials.toml"));
    credentials
        .set_api_key(Provider::OpenAiChat, "candidate-key")
        .expect("test key is saved");
    let ready = crate::providers::prepare(Provider::OpenAiChat, None, &|_| None, &credentials)
        .await
        .expect("saved key prepares the provider");
    let catalog = super::setup::default_catalog(&ready);
    let result = SetupResult {
        model: catalog.default_model().to_string(),
        catalog,
        ready,
        region: None,
        notice: String::new(),
    };

    assert_eq!(
        result.launch_model(Some("qwen3:8b")),
        "openai-chat/qwen3:8b",
        "a bare custom model is qualified with the provider"
    );
    assert_eq!(
        result.launch_model(Some("openai-chat/qwen3:8b")),
        "openai-chat/qwen3:8b",
        "a qualified custom model is kept as given"
    );
    assert_eq!(
        result.launch_model(None),
        result.model,
        "no requested model falls back to the setup default"
    );
}

#[tokio::test]
async fn connect_cancelled_writes_nothing() {
    let dir = tempfile::tempdir().expect("temporary directory is created");
    let host = DialogHost::default();
    let service = setup(&dir, host.clone());
    let task = tokio::spawn(async move { service.connect(None).await });
    let screen = wait_for(&host, "Welcome to Polaris Code").await;
    for label in [
        "Continue with ChatGPT",
        "OpenAI API key",
        "Anthropic API key",
        "AWS Bedrock",
    ] {
        assert!(screen.contains(label), "welcome screen offers {label}");
    }
    for (width, height) in [(120, 40), (80, 24), (40, 12)] {
        let rendered = draw(&host, width, height);
        assert!(
            rendered.contains("1. Continue with ChatGPT"),
            "first option fits at {width}x{height}"
        );
        assert!(
            rendered.contains("4. AWS Bedrock"),
            "last option fits at {width}x{height}"
        );
        assert_eq!(
            rendered.contains('█'),
            height >= 24,
            "brand art shows only when there is room at {width}x{height}"
        );
    }
    assert!(
        !screen.contains("OpenAI Chat Completions"),
        "Chat Completions is omitted from the login picker"
    );
    key(&host, KeyCode::Char('2'));
    wait_for(&host, "OpenAI API key").await;
    host.handle_event(&TuiEvent::Paste("do-not-save".into()));
    key(&host, KeyCode::Esc);
    wait_for(&host, "Welcome to Polaris Code").await;
    key(&host, KeyCode::Esc);
    let connected = task
        .await
        .expect("setup task completes")
        .expect("cancelled setup is not an error");
    assert!(connected.is_none(), "cancelled setup connects nothing");
    assert_eq!(
        std::fs::read_dir(dir.path())
            .expect("configuration directory is readable")
            .count(),
        0,
        "cancelled setup writes no files, including a discarded key"
    );
}

#[tokio::test]
async fn switching_provider_replaces_an_incompatible_default_model() {
    for provider in [Provider::Anthropic, Provider::OpenAi, Provider::OpenAiChat] {
        let dir = tempfile::tempdir().expect("temporary directory is created");
        let host = DialogHost::default();
        let model = format!("{}/selected-model", provider.registry_name());
        let selected = model.clone();
        let source = if provider == Provider::Anthropic {
            "openai/gpt-5.5"
        } else {
            "anthropic/claude-sonnet-5-5"
        };
        let service = setup(&dir, host.clone())
            .with_catalog(move |_| crate::config::ModelCatalog::new(source, [selected.clone()]));
        let task = tokio::spawn(async move { service.connect(Some(provider)).await });
        wait_for(&host, &format!("{} API key", provider.name())).await;
        enter(&host, "candidate-key").await;
        let result = task.await.unwrap().unwrap().expect("connection succeeds");
        assert_eq!(result.model, model);
        assert_eq!(result.catalog.select(None).unwrap(), model);
        assert_eq!(result.ready.report().provider(), provider);
        assert_eq!(
            CredentialStore::at(dir.path().join("credentials.toml"))
                .api_key(provider)
                .unwrap()
                .as_deref(),
            Some("candidate-key")
        );
    }
}

#[tokio::test]
async fn connect_rejected_catalog_persists_nothing() {
    let dir = tempfile::tempdir().expect("temporary directory is created");
    let host = DialogHost::default();
    let service = setup(&dir, host.clone())
        .with_catalog(|_| crate::config::ModelCatalog::pinned("anthropic/unavailable"));
    let task = tokio::spawn(async move { service.connect(Some(Provider::OpenAi)).await });
    wait_for(&host, "OpenAI API key").await;
    enter(&host, "candidate-key").await;
    wait_for(&host, "Couldn’t connect").await;
    key(&host, KeyCode::Esc);
    let connected = task
        .await
        .expect("setup task completes")
        .expect("abandoned setup is not an error");
    assert!(connected.is_none(), "rejected candidate connects nothing");
    assert!(
        CredentialStore::at(dir.path().join("credentials.toml"))
            .api_key(Provider::OpenAi)
            .expect("credential store is readable")
            .is_none(),
        "rejected candidate key is not persisted"
    );
}

#[tokio::test]
async fn connect_checked_refusal_keeps_saved_key() {
    let dir = tempfile::tempdir().expect("temporary directory is created");
    let credentials = CredentialStore::at(dir.path().join("credentials.toml"));
    credentials
        .set_api_key(Provider::OpenAi, "keep-original-key")
        .expect("original key is saved");
    let host = DialogHost::default();
    let service = setup(&dir, host.clone());
    let task = tokio::spawn(async move {
        service
            .connect_checked(Some(Provider::OpenAi), &|_, _, _| {
                Err("The candidate fails the session spend guard.".into())
            })
            .await
    });
    wait_for(&host, "Connect to OpenAI").await;
    key(&host, KeyCode::Char('2'));
    wait_for(&host, "OpenAI API key").await;
    enter(&host, "replacement-key").await;
    let screen = wait_for(&host, "Couldn’t connect").await;
    assert!(
        screen.contains("spend guard"),
        "failure screen shows the refusal reason"
    );
    key(&host, KeyCode::Esc);
    let connected = task
        .await
        .expect("setup task completes")
        .expect("abandoned setup is not an error");
    assert!(connected.is_none(), "refused candidate connects nothing");
    assert_eq!(
        credentials
            .api_key(Provider::OpenAi)
            .expect("credential store is readable")
            .as_deref(),
        Some("keep-original-key"),
        "refused candidate leaves the saved key in place"
    );
}
