//! `ChatGPT` sign-in, account storage, and the Responses API provider that
//! bills model usage to a `ChatGPT` plan.
//!
//! - `oauth`: browser sign-in, token refresh, identity verification, sign-out.
//! - `store`: the owner-only account file and its cross-process lock.
//! - `provider` and `responses`: the [`LlmProvider`](polaris::models::llm::LlmProvider)
//!   and its Responses API request encoding and stream decoding.
//! - `http`: bounded JSON bodies and error messages shared by the above.

mod http;
mod oauth;
mod provider;
pub(crate) mod responses;
mod store;

pub(crate) use oauth::ChatGptClient;
pub(crate) use provider::ChatGptProvider;
pub(crate) use store::{AccountSummary, ChatGptStore};

/// URL of the `ChatGPT` settings page for plan usage and app limits.
pub const USAGE_URL: &str = "https://chatgpt.com/#settings/Usage";

pub(crate) const RESOURCE: &str = "https://api.openai.com/v1";
pub(crate) const ISSUER: &str = "https://auth.openai.com";
pub(crate) const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";

/// A sign-in, account storage, or network error.
/// Network errors retain their source for retry classification.
#[derive(Debug, Clone)]
pub struct ChatGptError {
    message: String,
    kind: ErrorKind,
    source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ErrorKind {
    Account,
    Network,
    Expired,
    AuthorizationCodeRejected { client_id: String },
}

impl ChatGptError {
    pub(crate) fn message(message: String) -> Self {
        Self {
            message,
            kind: ErrorKind::Account,
            source: None,
        }
    }

    pub(super) fn network(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self {
            message: "could not reach OpenAI; check your connection and retry (saved credentials were kept)".into(),
            kind: ErrorKind::Network,
            source: Some(std::sync::Arc::new(source)),
        }
    }

    pub(super) fn identity_unavailable(
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: "OpenAI identity verification is temporarily unavailable; retry (saved credentials were kept)".into(),
            kind: ErrorKind::Network,
            source: Some(std::sync::Arc::new(source)),
        }
    }

    pub(super) fn code_rejected(client_id: &str) -> Self {
        Self {
            message: "ChatGPT rejected the authorization code during sign-in; start a fresh sign-in attempt".into(),
            kind: ErrorKind::AuthorizationCodeRejected { client_id: client_id.into() },
            source: None,
        }
    }

    pub(crate) fn retry_client_id(&self) -> Option<&str> {
        match &self.kind {
            ErrorKind::AuthorizationCodeRejected { client_id } => Some(client_id),
            _ => None,
        }
    }

    pub(super) fn expired() -> Self {
        Self {
            message: "ChatGPT session expired; sign in to ChatGPT again".into(),
            kind: ErrorKind::Expired,
            source: None,
        }
    }

    pub(super) fn from_generation(error: polaris::models::llm::GenerationError) -> Self {
        use polaris::models::llm::GenerationError;
        if matches!(
            error,
            GenerationError::Http(_)
                | GenerationError::Provider {
                    status: Some(500..=599),
                    ..
                }
        ) {
            Self::network(error)
        } else {
            Self {
                message: "OpenAI returned an invalid response (saved credentials were kept)".into(),
                kind: ErrorKind::Account,
                source: Some(std::sync::Arc::new(error)),
            }
        }
    }

    pub(super) fn is_network(&self) -> bool {
        self.kind == ErrorKind::Network
    }
    pub(crate) fn is_expired(&self) -> bool {
        self.kind == ErrorKind::Expired
    }

    pub(super) fn into_generation(self) -> polaris::models::llm::GenerationError {
        use polaris::models::llm::GenerationError;
        if self.is_network() {
            GenerationError::Provider {
                status: Some(503),
                message: self.to_string(),
                source: Some(Box::new(self)),
            }
        } else {
            GenerationError::Auth(self.to_string())
        }
    }
}

impl std::fmt::Display for ChatGptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ChatGptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|source| source as _)
    }
}

impl From<std::io::Error> for ChatGptError {
    fn from(err: std::io::Error) -> Self {
        Self {
            message: format!("cannot access private ChatGPT credentials: {err}"),
            kind: ErrorKind::Account,
            source: Some(std::sync::Arc::new(err)),
        }
    }
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn safe_label(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::ChatGptError;
    use crate::providers::{Provider, StartupError, account_startup_error};

    #[test]
    fn expired_credentials_offer_setup_without_hiding_other_failures() {
        let expired = account_startup_error(ChatGptError::expired());
        assert!(
            matches!(expired, StartupError::NotConfigured(Provider::ChatGpt)),
            "expired credentials offer setup on the launch that discovers expiry"
        );
        for error in [
            ChatGptError::network(std::io::Error::other("temporary service failure")),
            ChatGptError::from(std::io::Error::other("private storage inaccessible")),
            ChatGptError::message("invalid identity".into()),
        ] {
            let message = error.to_string();
            let startup = account_startup_error(error);
            assert!(
                !startup.needs_setup(),
                "only expiry triggers fresh setup: {message}"
            );
            assert_eq!(
                startup.to_string(),
                message,
                "other failures retain their diagnostic"
            );
            assert!(
                std::error::Error::source(&startup).is_some(),
                "other failures retain their source"
            );
        }
    }
}
