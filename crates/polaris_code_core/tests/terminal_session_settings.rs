//! Session setting commands against a fake configured model provider.

use polaris::models::ModelRegistry;
use polaris::models::llm::{GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream};
use polaris_code_core::agents::PermissionMode;
use polaris_code_core::config::ModelCatalog;
use polaris_code_core::terminal::{CommandOutcome, CommandRegistry, SessionSettings};
use polaris_tui::commands::CommandInvocation;

struct FakeProvider;

impl LlmProvider for FakeProvider {
    fn name(&self) -> &'static str {
        "fake"
    }

    async fn generate(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        unreachable!("settings tests never call the provider")
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        unreachable!("settings tests never call the provider")
    }
}

/// The models this host declares it can reach, of which only the `fake/*`
/// pair survives the provider restriction below.
const DECLARED: [&str; 3] = ["fake/one", "fake/two", "absent/three"];

fn fixture() -> (CommandRegistry, SessionSettings) {
    let mut models = ModelRegistry::new();
    models.register_llm_provider(FakeProvider);
    let catalog = ModelCatalog::new("fake/one", DECLARED)
        .restricted_to_providers(&models.llm_provider_names());
    let settings = SessionSettings::new("fake/one", PermissionMode::Confirm, &catalog);
    let commands = CommandRegistry::with_builtins();
    settings.register(&commands).unwrap();
    (commands, settings)
}

fn model_choices(commands: &CommandRegistry) -> Vec<String> {
    commands
        .specs()
        .into_iter()
        .find(|spec| spec.name == "model")
        .expect("`/model` is registered")
        .choices
}

#[tokio::test]
async fn valid_model_and_modes_change_the_next_snapshot() {
    let (commands, settings) = fixture();
    let outcome = commands
        .dispatch(&CommandInvocation::new("model", "fake/two"))
        .await;
    assert!(matches!(outcome, CommandOutcome::Reply(_)));
    assert_eq!(settings.snapshot().model_id, "fake/two");

    commands
        .dispatch(&CommandInvocation::new("permission", "auto-edit"))
        .await;
    assert_eq!(
        settings.snapshot().permission_mode,
        PermissionMode::AutoEdit
    );
}

#[tokio::test]
async fn the_chooser_offers_every_reachable_model_and_nothing_else() {
    let (commands, _) = fixture();
    let choices = model_choices(&commands);
    assert!(
        choices.len() > 1,
        "a host configured with more than one reachable model must offer more than one: {choices:?}"
    );
    assert_eq!(
        choices,
        ["fake/one", "fake/two"],
        "the chooser must offer the reachable set, not the compile-time active model"
    );
}

#[tokio::test]
async fn an_unknown_model_is_refused_with_the_set_that_would_have_worked() {
    let (commands, settings) = fixture();
    let CommandOutcome::Error(message) = commands
        .dispatch(&CommandInvocation::new("model", "fake/nine"))
        .await
    else {
        panic!("an unknown model must be refused before any turn runs");
    };
    assert!(message.contains("fake/one"), "{message}");
    assert!(message.contains("fake/two"), "{message}");
    assert_eq!(
        settings.snapshot().model_id,
        "fake/one",
        "a refused selection must not move the active model"
    );
}

#[tokio::test]
async fn a_model_whose_provider_is_not_configured_names_the_provider() {
    let (commands, _) = fixture();
    let CommandOutcome::Error(message) = commands
        .dispatch(&CommandInvocation::new("model", "absent/three"))
        .await
    else {
        panic!("a declared model with no registered provider is not reachable");
    };
    assert!(message.contains("absent"), "{message}");
    assert!(message.contains("fake/two"), "{message}");
}

#[tokio::test]
async fn a_bare_model_name_resolves_against_the_reachable_set() {
    let (commands, settings) = fixture();
    let outcome = commands
        .dispatch(&CommandInvocation::new("model", "two"))
        .await;
    assert!(matches!(outcome, CommandOutcome::Reply(_)), "{outcome:?}");
    assert_eq!(settings.snapshot().model_id, "fake/two");
}

#[tokio::test]
async fn invalid_changes_leave_the_active_configuration_valid() {
    let (commands, settings) = fixture();
    let before = settings.snapshot();
    assert!(matches!(
        commands
            .dispatch(&CommandInvocation::new("model", "missing/two"))
            .await,
        CommandOutcome::Error(_)
    ));
    assert!(matches!(
        commands
            .dispatch(&CommandInvocation::new("permission", "unattended"))
            .await,
        CommandOutcome::Error(_)
    ));
    assert_eq!(settings.snapshot(), before);
}

#[tokio::test]
async fn empty_commands_report_active_values_and_advertise_only_approved_modes() {
    let (commands, _) = fixture();
    for name in ["model", "permission"] {
        assert!(matches!(
            commands.dispatch(&CommandInvocation::new(name, "")).await,
            CommandOutcome::Reply(_)
        ));
    }
    let CommandOutcome::Reply(listing) = commands
        .dispatch(&CommandInvocation::new("model", ""))
        .await
    else {
        unreachable!("checked above")
    };
    assert!(listing.contains("`fake/one` (active)"), "{listing}");
    assert!(listing.contains("`fake/two`"), "{listing}");
    let permission = commands
        .specs()
        .into_iter()
        .find(|spec| spec.name == "permission")
        .unwrap();
    assert_eq!(permission.choices, ["confirm", "auto-edit", "plan"]);
}
