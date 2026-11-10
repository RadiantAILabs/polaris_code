use super::responses::{reasoning_scope, request_body, response_stream};
use super::{ChatGptClient, ChatGptError, RESOURCE};
use polaris::models::llm::{
    GenerationError, LlmProvider, LlmRequest, LlmResponse, LlmStream, StreamEventExt,
};

/// Sends Responses API requests using one account's `ChatGPT` authorization.
#[derive(Clone)]
pub struct ChatGptProvider {
    client: ChatGptClient,
    account_id: String,
    reasoning_scope: String,
}

impl std::fmt::Debug for ChatGptProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptProvider").finish_non_exhaustive()
    }
}

impl ChatGptProvider {
    /// Create a provider bound to this account registration.
    pub fn new(client: ChatGptClient, account_id: impl Into<String>) -> Self {
        let account_id = account_id.into();
        let reasoning_scope = reasoning_scope(&account_id);
        Self {
            client,
            account_id,
            reasoning_scope,
        }
    }

    async fn send(
        &self,
        body: &serde_json::Value,
    ) -> Result<(reqwest::Response, String), GenerationError> {
        let access = self
            .client
            .access_token(&self.account_id)
            .await
            .map_err(ChatGptError::into_generation)?;
        let response = self
            .client
            .inference
            .post(format!("{RESOURCE}/responses"))
            .bearer_auth(&access)
            .json(body)
            .send()
            .await
            .map_err(|error| ChatGptError::network(error).into_generation())?;
        Ok((response, access))
    }
}

impl LlmProvider for ChatGptProvider {
    fn name(&self) -> &'static str {
        "chatgpt"
    }

    async fn generate(
        &self,
        model: &str,
        request: LlmRequest,
    ) -> Result<LlmResponse, GenerationError> {
        self.stream(model, request).await?.collect_response().await
    }

    async fn stream(&self, model: &str, request: LlmRequest) -> Result<LlmStream, GenerationError> {
        let body = request_body(model, &request, &self.reasoning_scope)?;
        let (mut response, access) = self.send(&body).await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            // The saved token was revoked before its expiry. Refresh it once;
            // a second rejection is reported as an authentication failure.
            self.client
                .invalidate_access(&self.account_id, &access)
                .await
                .map_err(ChatGptError::into_generation)?;
            response = self.send(&body).await?.0;
        }
        response_stream(response, self.reasoning_scope.clone()).await
    }

    fn endpoint(&self) -> Option<String> {
        Some(RESOURCE.into())
    }
}
