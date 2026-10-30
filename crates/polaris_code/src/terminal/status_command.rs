//! Read-only launch and session diagnostics through the local command registry.

use std::path::Path;

use polaris_code::providers::ProviderReport;
use polaris_code::terminal::{
    CommandError, CommandOutcome, CommandRegistry, SessionNavigation, SessionSettings,
};
use polaris_tui::commands::CommandSpec;

use super::Startup;

pub(super) fn register(
    registry: &CommandRegistry,
    settings: &SessionSettings,
    navigation: &SessionNavigation,
    workspace: &Path,
    startup: Startup,
    report: ProviderReport,
) -> Result<(), CommandError> {
    let settings = settings.clone();
    let navigation = navigation.clone();
    let workspace = workspace.to_path_buf();
    registry.register_sync(
        CommandSpec::new("status", "Show configuration, credentials source, and session status"),
        move |_, args| {
            if !args.trim().is_empty() {
                return CommandOutcome::Error("Usage: /status".to_string());
            }
            let snapshot = settings.snapshot();
            let permission = snapshot.permission_mode.permission_policy();
            let ceiling = settings.permission_ceiling().map_or("none", |ceiling| ceiling.label());
            let region = report.region().unwrap_or("not applicable");
            let endpoint = report.endpoint().unwrap_or("provider default");
            let files = if startup.files.is_empty() { "none".to_string() } else {
                startup.files.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", ")
            };
            let config_directory = config_directory_label(&startup);
            let dotenv = startup.dotenv.as_ref().map_or_else(|| "none".to_string(), |path| path.display().to_string());
            CommandOutcome::Reply(format!(
                "- Version: {}\n- Provider: {}\n- Model: {}\n- Region: {region}\n- Endpoint: {endpoint}\n- Permission mode: {}\n- Permission ceiling: {ceiling}\n- Key source: {}\n- Config directory: {config_directory}\n- Config files: {files}\n- Profile: {}\n- Environment file: {dotenv}\n- Session: {}\n- Workspace: {}",
                env!("CARGO_PKG_VERSION"), report.provider().flag(), snapshot.model_id, polaris_code::terminal::status::permission_posture(&permission),
                report.credential_source(), startup.profile.as_deref().unwrap_or("none"), navigation.current(), workspace.display(),
            ))
        },
    )
}

fn config_directory_label(startup: &Startup) -> String {
    startup.config_directory.as_ref().map_or_else(
        || "unavailable".to_string(),
        |directory| {
            let source = if startup.config_dir_explicit {
                "POLARIS_CONFIG_DIR"
            } else {
                "XDG/home fallback"
            };
            format!("{} (source: {source})", directory.display())
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_reads_current_session_settings_and_navigation() {
        use polaris::sessions::{SessionId, SessionsAPI, store::memory::InMemoryStore};
        use polaris_code::{
            agents::{PermissionCeiling, PermissionMode},
            config::ModelCatalog,
        };
        use polaris_tui::commands::CommandInvocation;
        use std::sync::Arc;

        let catalog = ModelCatalog::new("fake/first", ["fake/second"]);
        let settings = SessionSettings::new("fake/first", PermissionMode::Plan, &catalog)
            .with_permission_ceiling(PermissionCeiling::new(PermissionMode::Confirm).unwrap());
        let navigation = SessionNavigation::new(
            SessionsAPI::new(Arc::new(InMemoryStore::new())),
            SessionId::from_string("first-session"),
        );
        let commands = CommandRegistry::new();
        register(
            &commands,
            &settings,
            &navigation,
            Path::new("/workspace"),
            Startup {
                files: vec!["/workspace/polaris.toml".into()],
                config_directory: Some("/work-setup".into()),
                config_dir_explicit: true,
                ..Startup::default()
            },
            ProviderReport::new(
                polaris_code::providers::Provider::Anthropic,
                None,
                Some("https://gateway.example/v1".to_string()),
                "stored key (polaris login)",
            ),
        )
        .unwrap();
        let CommandOutcome::Reply(first) = commands
            .dispatch(&CommandInvocation::new("status", ""))
            .await
        else {
            panic!("expected local reply");
        };
        for expected in [
            "Model: fake/first",
            "Permission mode: plan",
            "Permission ceiling: confirm",
            "Workspace: /workspace",
            "Config files: /workspace/polaris.toml",
            "Config directory: /work-setup (source: POLARIS_CONFIG_DIR)",
            "Provider: anthropic",
            "Key source: stored key (polaris login)",
            "Endpoint: https://gateway.example/v1",
        ] {
            assert!(first.contains(expected), "{first}");
        }
        assert!(first.contains(&format!("Session: {}", navigation.current())));
        settings.reset("fake/second");
        navigation.set_current(SessionId::from_string("second-session"));
        let CommandOutcome::Reply(second) = commands
            .dispatch(&CommandInvocation::new("status", ""))
            .await
        else {
            panic!("expected local reply");
        };
        for expected in ["Model: fake/second", "Permission mode: confirm"] {
            assert!(second.contains(expected), "{second}");
        }
        assert!(second.contains(&format!("Session: {}", navigation.current())));
        assert!(matches!(
            commands
                .dispatch(&CommandInvocation::new("status", "extra"))
                .await,
            CommandOutcome::Error(_)
        ));
    }

    #[tokio::test]
    async fn status_uses_the_resolved_bedrock_region_and_role() {
        use polaris::sessions::{SessionId, SessionsAPI, store::memory::InMemoryStore};
        use polaris_code::{agents::PermissionMode, config::ModelCatalog, providers::Provider};
        use polaris_tui::commands::CommandInvocation;
        use std::sync::Arc;

        let model = "aws.bedrock/global.anthropic.claude-sonnet-5";
        let catalog = ModelCatalog::new(model, [model]);
        let settings = SessionSettings::new(model, PermissionMode::Confirm, &catalog);
        let navigation = SessionNavigation::new(
            SessionsAPI::new(Arc::new(InMemoryStore::new())),
            SessionId::from_string("session"),
        );
        let commands = CommandRegistry::new();
        register(
            &commands,
            &settings,
            &navigation,
            Path::new("/workspace"),
            Startup::default(),
            ProviderReport::new(
                Provider::Bedrock,
                Some("eu-west-1".into()),
                None,
                "EC2 instance role",
            ),
        )
        .unwrap();
        let CommandOutcome::Reply(reply) = commands
            .dispatch(&CommandInvocation::new("status", ""))
            .await
        else {
            panic!("expected a local reply");
        };
        for expected in [
            "Provider: bedrock",
            "Region: eu-west-1",
            "Key source: EC2 instance role",
        ] {
            assert!(reply.contains(expected), "{reply}");
        }
        assert!(!reply.contains("not resolved"), "{reply}");
    }

    #[test]
    fn config_directory_status_distinguishes_override_and_fallback() {
        let mut startup = Startup {
            config_directory: Some("/custom/polaris".into()),
            ..Startup::default()
        };
        assert_eq!(
            config_directory_label(&startup),
            "/custom/polaris (source: XDG/home fallback)"
        );
        startup.config_dir_explicit = true;
        assert_eq!(
            config_directory_label(&startup),
            "/custom/polaris (source: POLARIS_CONFIG_DIR)"
        );
        startup.config_directory = None;
        assert_eq!(config_directory_label(&startup), "unavailable");
    }
}
