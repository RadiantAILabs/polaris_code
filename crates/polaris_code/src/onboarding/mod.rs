//! Connection setup for startup, CLI login, and interactive sessions.
//!
//! [`SetupService`] prepares connections through host dialogs. The prepared
//! connection is served by [`crate::connection`]; in-session account commands
//! live in [`crate::terminal::SessionAccounts`].

mod brand;
mod browser;
pub(crate) mod prompts;
mod setup;
mod terminal;

#[cfg(test)]
mod tests;

pub use setup::{SetupError, SetupResult, SetupService};

/// Run connection setup in a temporary full-screen terminal.
///
/// Call before starting the main TUI. Returns `Ok(None)` if setup is cancelled.
///
/// # Errors
///
/// Returns terminal, credential storage, or provider setup errors.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> Result<(), polaris_code::onboarding::SetupError> {
/// if let Some(result) = polaris_code::onboarding::run_terminal(None).await? {
///     let model = result.model;
/// }
/// # Ok(())
/// # }
/// ```
pub async fn run_terminal(
    provider: Option<crate::providers::Provider>,
) -> Result<Option<SetupResult>, SetupError> {
    run_terminal_with_catalog(provider, setup::default_catalog).await
}

/// Run terminal setup using the host's model catalog configuration.
///
/// # Errors
/// Returns credential, provider, or terminal setup failures.
pub async fn run_terminal_with_catalog(
    provider: Option<crate::providers::Provider>,
    catalog: impl Fn(&crate::providers::ReadyProvider) -> crate::config::ModelCatalog
    + Send
    + Sync
    + 'static,
) -> Result<Option<SetupResult>, SetupError> {
    let host = polaris_tui::dialogs::DialogHost::default();
    let service = SetupService::user(host.clone())?
        .with_catalog(catalog)
        .with_clipboard(|text| async move { terminal::copy_to_clipboard(&text) });
    terminal::run(&host, service.connect(provider))
        .await
        .map_err(SetupError::from)?
}
