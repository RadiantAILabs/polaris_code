//! A `/model` choice outlives `/new` and `/resume` for the life of the process,
//! while the permission posture still falls back to its safe default.

use polaris::models::ModelRegistry;
use polaris::models::llm::{GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream};
use polaris_code::agents::PermissionMode;
use polaris_code::config::ModelCatalog;
use polaris_code::terminal::{CommandOutcome, CommandRegistry, SessionSettings};
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
        unreachable!("settings never call the model")
    }

    async fn stream(
        &self,
        _model: &str,
        _request: LlmRequest,
    ) -> Result<LlmStream, GenerationError> {
        unreachable!("settings never call the model")
    }
}

/// A host launched on `fake/sonnet-5-5`, which can reach three models.
fn fixture() -> (CommandRegistry, SessionSettings) {
    let mut models = ModelRegistry::new();
    models.register_llm_provider(FakeProvider);
    let catalog = ModelCatalog::new(
        "fake/sonnet-5-5",
        ["fake/sonnet-5-5", "fake/opus-5-5", "fake/haiku-5-5"],
    )
    .restricted_to_providers(&models.llm_provider_names());
    let settings = SessionSettings::new("fake/sonnet-5-5", PermissionMode::Confirm, &catalog);
    let commands = CommandRegistry::with_builtins();
    settings.register(&commands).expect("settings register");
    (commands, settings)
}

async fn run(commands: &CommandRegistry, name: &str, args: &str) -> CommandOutcome {
    commands.dispatch(&CommandInvocation::new(name, args)).await
}

#[tokio::test]
async fn a_model_chosen_with_the_command_survives_a_new_session() {
    let (commands, settings) = fixture();
    run(&commands, "model", "opus").await;
    assert_eq!(settings.snapshot().model_id, "fake/opus-5-5");

    settings.reset_keeping_model();

    assert_eq!(settings.snapshot().model_id, "fake/opus-5-5");
}

#[tokio::test]
async fn the_model_survives_every_session_switch_not_only_the_first() {
    let (commands, settings) = fixture();
    run(&commands, "model", "haiku").await;
    for _ in 0..3 {
        settings.reset_keeping_model();
    }
    assert_eq!(settings.snapshot().model_id, "fake/haiku-5-5");

    run(&commands, "model", "fake/sonnet-5-5").await;
    settings.reset_keeping_model();
    assert_eq!(settings.snapshot().model_id, "fake/sonnet-5-5");
}

#[tokio::test]
async fn a_new_session_still_starts_in_the_safe_permission_mode() {
    let (commands, settings) = fixture();
    run(&commands, "model", "opus").await;
    run(&commands, "permission", "plan").await;
    assert_eq!(settings.snapshot().permission_mode, PermissionMode::Plan);

    settings.reset_keeping_model();

    assert_eq!(settings.snapshot().permission_mode, PermissionMode::Confirm);
    assert_eq!(settings.snapshot().model_id, "fake/opus-5-5");
}

#[tokio::test]
async fn a_new_session_forgets_which_model_served_the_last_turn() {
    let (commands, settings) = fixture();
    run(&commands, "model", "opus").await;
    settings.note_served("fake/opus-5-5");

    settings.reset_keeping_model();

    assert_eq!(settings.served_model(), None);
}

#[tokio::test]
async fn resetting_to_a_named_model_still_puts_that_model_back() {
    let (commands, settings) = fixture();
    run(&commands, "model", "opus").await;

    settings.reset("fake/sonnet-5-5");

    assert_eq!(settings.snapshot().model_id, "fake/sonnet-5-5");
}

#[tokio::test]
async fn a_family_name_says_which_model_it_chose_and_keeps_the_note_out_of_exact_choices() {
    let (commands, settings) = fixture();
    let CommandOutcome::Reply(exact) = run(&commands, "model", "fake/opus-5-5").await else {
        panic!("an exact id is accepted");
    };
    assert_eq!(exact, "Model set to `fake/opus-5-5` for the next turn.");

    // `5-5` appears in every id, in three families: nothing to choose between.
    let CommandOutcome::Error(refused) = run(&commands, "model", "5-5").await else {
        panic!("a name that spans families is refused");
    };
    assert!(refused.contains("ambiguous"), "{refused}");
    assert!(
        refused.contains("the active model was not changed"),
        "{refused}"
    );
    assert_eq!(settings.snapshot().model_id, "fake/opus-5-5");
}

#[tokio::test]
async fn the_command_names_the_newest_match_when_a_family_has_several() {
    let mut models = ModelRegistry::new();
    models.register_llm_provider(FakeProvider);
    let catalog = ModelCatalog::new(
        "fake/sonnet-4-6",
        ["fake/sonnet-4-6", "fake/sonnet-5", "fake/sonnet-5-5"],
    )
    .restricted_to_providers(&models.llm_provider_names());
    let settings = SessionSettings::new("fake/sonnet-4-6", PermissionMode::Confirm, &catalog);
    let commands = CommandRegistry::with_builtins();
    settings.register(&commands).expect("settings register");

    let CommandOutcome::Reply(reply) = run(&commands, "model", "sonnet").await else {
        panic!("a family name is accepted");
    };

    assert_eq!(settings.snapshot().model_id, "fake/sonnet-5-5");
    assert!(reply.contains("`sonnet` matches 3 models"), "{reply}");
    assert!(reply.contains("the newest, `fake/sonnet-5-5`"), "{reply}");
}
