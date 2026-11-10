//! Bounded JSON bodies and readable error messages for `OpenAI` HTTP responses.

use polaris::models::llm::GenerationError;
use serde_json::Value;

/// Largest JSON body read from an authorization or model service response.
const MAX_JSON_BYTES: usize = 1024 * 1024;

/// Longest server-supplied error detail kept in a message, in characters.
const MAX_DETAIL_CHARS: usize = 240;

/// Read and decode a JSON body of at most [`MAX_JSON_BYTES`].
pub(crate) async fn bounded_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, GenerationError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| GenerationError::Http("the model service response was interrupted".into()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_JSON_BYTES {
            return Err(GenerationError::InvalidResponse(
                "the service JSON response exceeds 1 MiB".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| GenerationError::InvalidResponse("the service returned invalid JSON".into()))
}

/// The service's `error.message`, redacted and cut to one short line.
pub(crate) fn server_detail(body: &Value) -> Option<String> {
    let message = body["error"]["message"].as_str()?;
    let redacted = crate::redaction::SecretRedactor::new(true).scan(message);
    let line = redacted
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let detail: String = line
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DETAIL_CHARS)
        .collect();
    (!detail.is_empty()).then_some(detail)
}

/// Describe a failed non-streaming response for the user.
pub(crate) async fn response_error(response: reqwest::Response) -> String {
    let status = response.status();
    let body: Value = bounded_json(response).await.unwrap_or_default();
    let code = body["error"]["code"].as_str().unwrap_or("");
    if code.is_empty() {
        let message = format!("the model service returned HTTP {status}");
        match server_detail(&body) {
            Some(detail) => format!("{message}: {detail}"),
            None => message,
        }
    } else {
        super::responses::error_message(code)
    }
}

#[cfg(test)]
mod tests {
    use super::server_detail;
    use serde_json::json;

    #[test]
    fn server_details_accept_unicode_pem_labels_without_panicking() {
        let label = format!("a{}", "😀".repeat(12));
        let message = format!("-----BEGIN {label}PRIVATE KEY-----");
        let detail = server_detail(&json!({"error": {"message": message}}));
        assert_eq!(
            detail.as_deref(),
            Some(message.as_str()),
            "a malformed PEM header is ordinary text"
        );

        let message = format!(
            "-----BEGIN RSA PRIVATE KEY-----\nsecret-body\n-----END {label}PRIVATE KEY-----"
        );
        assert_eq!(
            server_detail(&json!({"error": {"message": message}})).as_deref(),
            Some("[REDACTED:private-key]"),
            "an unusable closing label still withholds the private-key body"
        );
    }
}
