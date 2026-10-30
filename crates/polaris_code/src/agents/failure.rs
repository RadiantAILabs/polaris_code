//! What a failed model call means, and how to say it in one line.
//!
//! A provider error reaches the agent as a [`GenerationError`]: a typed value
//! whose `Display` is meant for logs, and which for an HTTP failure embeds the
//! provider's raw JSON body. This module is the single place that turns one into
//!
//! - a [`FailureKind`], the small closed set the retry policy and the headless
//!   `error_kind` field both speak, and
//! - a [`TurnFailure`], the one short, actionable sentence a user sees and the
//!   kind it was derived from.
//!
//! The raw error is never discarded here; callers log it with `tracing` before
//! they report the summary.
//!
//! # Classification
//!
//! Variants that already carry the answer ([`GenerationError::Auth`],
//! [`GenerationError::RateLimited`], [`GenerationError::Http`]) are taken as
//! they are. The Anthropic client does not map its HTTP failures onto them: every
//! non-2xx response arrives as [`GenerationError::Provider`] with the status and
//! the raw body, and an error event inside a stream arrives with no status and
//! the text `<error_type>: <message>`. So a `Provider` error is classified by
//! the retained typed Bedrock error, then HTTP status and the provider's own
//! structured error type. Diagnostic prose never determines Bedrock retries. A framework that maps these itself changes nothing here:
//! the typed variants are read first.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use polaris::models::llm::GenerationError;
use serde::Serialize;

use crate::providers::Provider;

/// The longest provider detail a one-line message carries, in characters.
const DETAIL_LIMIT: usize = 240;

/// What kind of failure a model call ended in.
///
/// The set is deliberately small and stable: it is the `error_kind` a headless
/// run reports and the axis the retry policy decides on. [`Auth`](Self::Auth),
/// [`InvalidRequest`](Self::InvalidRequest) and [`Other`](Self::Other) are never
/// retried; the rest are transient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The provider rejected the credentials (HTTP 401 or 403).
    Auth,
    /// The provider is limiting the request rate (HTTP 429).
    RateLimit,
    /// The provider failed on its side: HTTP 5xx, including 529 "overloaded".
    Overloaded,
    /// No response arrived in time.
    Timeout,
    /// The provider could not be reached at all.
    Network,
    /// The provider rejected the request itself (other 4xx), for example a
    /// prompt over the context window.
    InvalidRequest,
    /// Anything else: a malformed response, a refusal, an unrecognised error.
    Other,
}

impl FailureKind {
    /// The stable lowercase name of this kind, as `error_kind` reports it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::RateLimit => "rate_limit",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::InvalidRequest => "invalid_request",
            Self::Other => "other",
        }
    }

    /// Whether the same request can reasonably succeed if sent again.
    #[must_use]
    pub const fn is_transient(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Overloaded | Self::Timeout | Self::Network
        )
    }
}

impl fmt::Display for FailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// How a failed turn is reported: its kind and the one message a user reads.
///
/// Carried from the agent's `act` step, through the `recover` handler and
/// [`LoopState::failure`](super::LoopState::failure), to the run's
/// [`TurnReason::Failed`](crate::run::TurnReason::Failed). The message is
/// already final: front ends print it as it is and do not rebuild it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnFailure {
    /// The kind of failure, for callers that branch on it.
    pub kind: FailureKind,
    /// The short, human-readable summary.
    pub message: String,
}

impl TurnFailure {
    /// A failure of `kind` described by `message`.
    #[must_use]
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for TurnFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for TurnFailure {
    fn from(message: String) -> Self {
        Self::new(FailureKind::Other, message)
    }
}

impl From<&str> for TurnFailure {
    fn from(message: &str) -> Self {
        Self::new(FailureKind::Other, message)
    }
}

/// What the retry loop spent on a call before it gave up, so the final message
/// can say so.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Attempts {
    /// How many times the call was sent again after a failure.
    pub retries: u32,
    /// How long the whole call took, waits included.
    pub elapsed: Duration,
    /// Whether the failed attempt had already shown output to the user, which
    /// is why it could not be sent again.
    pub committed: bool,
}

/// A model call that produced nothing for `waited` and was abandoned.
///
/// Carried as the source of a [`GenerationError::Provider`] built by
/// [`timed_out`], because the framework's [`GenerationError::Http`] holds only
/// text and so cannot be told apart from a refused connection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttemptTimedOut {
    waited: Duration,
}

impl fmt::Display for AttemptTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no response for {}", seconds(self.waited))
    }
}

impl Error for AttemptTimedOut {}

/// The error for a call that was given up on after waiting `waited`.
pub(crate) fn timed_out(waited: Duration) -> GenerationError {
    let cause = AttemptTimedOut { waited };
    GenerationError::Provider {
        status: None,
        message: cause.to_string(),
        source: Some(Box::new(cause)),
    }
}

/// How long the provider asked the caller to wait before trying again.
pub(crate) fn requested_wait(error: &GenerationError) -> Option<Duration> {
    match error {
        GenerationError::RateLimited { retry_after } => *retry_after,
        _ => None,
    }
}

/// Classify `error` into the kind the retry policy and reports use.
#[must_use]
pub fn classify(error: &GenerationError) -> FailureKind {
    match error {
        GenerationError::Auth(_) => FailureKind::Auth,
        GenerationError::RateLimited { .. } => FailureKind::RateLimit,
        GenerationError::Http(_) => FailureKind::Network,
        GenerationError::InvalidRequest(_)
        | GenerationError::UnsupportedContent(_)
        | GenerationError::UnsupportedOperation(_) => FailureKind::InvalidRequest,
        GenerationError::Provider {
            status,
            message,
            source,
        } => {
            if source
                .as_deref()
                .is_some_and(|source| source.is::<AttemptTimedOut>())
            {
                return FailureKind::Timeout;
            }
            if let Some(kind) = source.as_deref().and_then(classify_bedrock) {
                return kind;
            }
            classify_provider(*status, &Wire::parse(message))
        }
        _ => FailureKind::Other,
    }
}

/// The framework boxes the modeled operation/event error, not the SDK wrapper
/// (`polaris_model_providers` 0.7.1, `bedrock::provider::map_sdk_error`). Downcast
/// each of the three published SDK types that adapter can retain.
fn classify_bedrock(source: &(dyn Error + Send + Sync + 'static)) -> Option<FailureKind> {
    use aws_sdk_bedrockruntime::operation::converse::ConverseError;
    use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
    use aws_sdk_bedrockruntime::types::error::ConverseStreamOutputError;

    if let Some(error) = source.downcast_ref::<ConverseError>() {
        return Some(match error {
            ConverseError::ModelTimeoutException(_) => FailureKind::Timeout,
            ConverseError::ServiceUnavailableException(_)
            | ConverseError::InternalServerException(_)
            | ConverseError::ModelNotReadyException(_) => FailureKind::Overloaded,
            ConverseError::ThrottlingException(_) => FailureKind::RateLimit,
            ConverseError::AccessDeniedException(_) => FailureKind::Auth,
            ConverseError::ValidationException(_) | ConverseError::ResourceNotFoundException(_) => {
                FailureKind::InvalidRequest
            }
            _ => FailureKind::Other,
        });
    }
    if let Some(error) = source.downcast_ref::<ConverseStreamError>() {
        return Some(match error {
            ConverseStreamError::ModelTimeoutException(_) => FailureKind::Timeout,
            ConverseStreamError::ServiceUnavailableException(_)
            | ConverseStreamError::InternalServerException(_)
            | ConverseStreamError::ModelNotReadyException(_) => FailureKind::Overloaded,
            ConverseStreamError::ThrottlingException(_) => FailureKind::RateLimit,
            ConverseStreamError::AccessDeniedException(_) => FailureKind::Auth,
            ConverseStreamError::ValidationException(_)
            | ConverseStreamError::ResourceNotFoundException(_) => FailureKind::InvalidRequest,
            _ => FailureKind::Other,
        });
    }
    source
        .downcast_ref::<ConverseStreamOutputError>()
        .map(|error| match error {
            ConverseStreamOutputError::ServiceUnavailableException(_)
            | ConverseStreamOutputError::InternalServerException(_) => FailureKind::Overloaded,
            ConverseStreamOutputError::ThrottlingException(_) => FailureKind::RateLimit,
            ConverseStreamOutputError::ValidationException(_) => FailureKind::InvalidRequest,
            _ => FailureKind::Other,
        })
}

/// Classify a [`GenerationError::Provider`] by status, then by what the
/// provider said about itself.
fn classify_provider(status: Option<u16>, wire: &Wire) -> FailureKind {
    match status {
        Some(401 | 403) => FailureKind::Auth,
        Some(408 | 504) => FailureKind::Timeout,
        Some(429) => FailureKind::RateLimit,
        Some(500..=599) => FailureKind::Overloaded,
        Some(400..=499) => FailureKind::InvalidRequest,
        _ => wire.kind(),
    }
}

/// What a provider said about a failure, when its message carries it.
struct Wire {
    /// The provider's error type, such as `overloaded_error`.
    error_type: Option<String>,
    /// The provider's own message, or the whole text when none could be read.
    message: String,
}

impl Wire {
    /// Read the two shapes the Anthropic path produces: an HTTP error body
    /// (`{"type":"error","error":{"type":"…","message":"…"}}`) and a stream
    /// error event (`<error_type>: <message>`). Any other text is kept whole.
    fn parse(text: &str) -> Self {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text)
            && let Some(error) = value.get("error")
        {
            let field = |name: &str| error.get(name).and_then(|v| v.as_str()).map(str::to_string);
            let (error_type, message) = (field("type"), field("message"));
            if error_type.is_some() || message.is_some() {
                return Self {
                    error_type,
                    message: message.unwrap_or_else(|| text.to_string()),
                };
            }
        }
        if let Some((error_type, message)) = text.split_once(": ")
            && error_type.ends_with("_error")
            && !error_type.contains(char::is_whitespace)
        {
            return Self {
                error_type: Some(error_type.to_string()),
                message: message.to_string(),
            };
        }
        Self {
            error_type: None,
            message: text.to_string(),
        }
    }

    /// The kind this text implies when no HTTP status settled it.
    fn kind(&self) -> FailureKind {
        if let Some(error_type) = &self.error_type {
            match error_type.as_str() {
                "authentication_error" | "permission_error" => return FailureKind::Auth,
                "rate_limit_error" => return FailureKind::RateLimit,
                "overloaded_error" | "api_error" => return FailureKind::Overloaded,
                "timeout_error" => return FailureKind::Timeout,
                "invalid_request_error" | "not_found_error" | "request_too_large" => {
                    return FailureKind::InvalidRequest;
                }
                _ => {}
            }
        }
        FailureKind::Other
    }
}

/// Summarise `error` as the one message a user reads about a failed call to
/// the model `model_id`, after `attempts` of retrying.
///
/// The message names the provider, says what was wrong in its own words where
/// those help (the status, the error type, a rejected-request reason), says what
/// to do where the user can do something, and — when the call was retried —
/// says how hard it tried. It never contains a raw response body, a node id or
/// a duration from the graph executor.
#[must_use]
pub fn describe(error: &GenerationError, model_id: &str, attempts: Attempts) -> TurnFailure {
    let kind = classify(error);
    let provider = Provider::of_model(model_id);
    let prefix = model_id.split_once('/').map_or("", |(prefix, _)| prefix);
    let who = match provider {
        Some(provider) => provider.name(),
        None if prefix.is_empty() => "The model provider",
        None => prefix,
    };
    let (status, wire) = match error {
        GenerationError::Provider {
            status, message, ..
        } => (*status, Wire::parse(message)),
        _ => (None, Wire::parse("")),
    };
    let tag = tag(status, wire.error_type.as_deref());
    let detail = oneline(&match error {
        GenerationError::Provider { .. } => wire.message.clone(),
        GenerationError::Http(text)
        | GenerationError::Auth(text)
        | GenerationError::InvalidRequest(text)
        | GenerationError::UnsupportedContent(text)
        | GenerationError::UnsupportedOperation(text) => text.clone(),
        other => other.to_string(),
    });

    let mut message = match kind {
        FailureKind::Auth => format!(
            "{who} rejected the {}{tag}. {}",
            provider.map_or("credentials", Provider::credential),
            provider.map_or_else(
                || "Check the provider credentials.".to_string(),
                Provider::credential_hint
            )
        ),
        FailureKind::RateLimit => match requested_wait(error) {
            Some(wait) => format!(
                "{who} rate limited the request{tag} and asked for a wait of {}.",
                seconds(wait)
            ),
            None => format!("{who} rate limited the request{tag}."),
        },
        FailureKind::Overloaded => format!("{who} is overloaded or unavailable{tag}."),
        FailureKind::Timeout => match timed_out_after(error) {
            Some(waited) => format!("No response from {who} for {}.", seconds(waited)),
            None => format!("{who} timed out{tag}."),
        },
        FailureKind::Network => format!(
            "Could not reach {who}: {detail}. {}",
            provider.map_or(
                "Check your network connection and proxy settings.",
                Provider::network_hint
            )
        ),
        FailureKind::InvalidRequest => format!("{who} rejected the request{tag}: {detail}"),
        FailureKind::Other => format!("{who} request failed{tag}: {detail}"),
    };
    if attempts.committed && kind.is_transient() {
        message.push_str(" The response had already started, so it was not retried.");
    } else if attempts.retries > 0 {
        let noun = if attempts.retries == 1 {
            "retry"
        } else {
            "retries"
        };
        message.push_str(&format!(
            " Gave up after {} {noun} over {}.",
            attempts.retries,
            seconds(attempts.elapsed)
        ));
    }
    TurnFailure::new(kind, message)
}

/// The wait a [`timed_out`] error recorded, if `error` is one.
fn timed_out_after(error: &GenerationError) -> Option<Duration> {
    match error {
        GenerationError::Provider { source, .. } => source
            .as_deref()
            .and_then(|source| source.downcast_ref::<AttemptTimedOut>())
            .map(|timeout| timeout.waited),
        _ => None,
    }
}

/// The parenthetical that names the HTTP status and the provider's error type,
/// or nothing when neither is known.
fn tag(status: Option<u16>, error_type: Option<&str>) -> String {
    let parts: Vec<String> = status
        .map(|status| format!("HTTP {status}"))
        .into_iter()
        .chain(error_type.map(str::to_string))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// `text` on one line, cut to [`DETAIL_LIMIT`] characters.
fn oneline(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= DETAIL_LIMIT {
        return joined;
    }
    let mut cut: String = joined.chars().take(DETAIL_LIMIT).collect();
    cut.push('…');
    cut
}

/// Whole seconds, rounded up so a short wait never reads as zero.
fn seconds(duration: Duration) -> String {
    let secs = duration.as_secs() + u64::from(duration.subsec_nanos() > 0);
    format!("{secs} s")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC: &str = "anthropic/claude-sonnet-4-6";

    fn provider_error(status: Option<u16>, message: &str) -> GenerationError {
        GenerationError::Provider {
            status,
            message: message.to_string(),
            source: None,
        }
    }

    fn body(error_type: &str, message: &str) -> String {
        serde_json::json!({
            "type": "error",
            "error": {"type": error_type, "message": message},
            "request_id": "req_011",
        })
        .to_string()
    }

    #[test]
    fn http_status_decides_the_kind() {
        for (status, expected) in [
            (401, FailureKind::Auth),
            (403, FailureKind::Auth),
            (408, FailureKind::Timeout),
            (429, FailureKind::RateLimit),
            (400, FailureKind::InvalidRequest),
            (404, FailureKind::InvalidRequest),
            (413, FailureKind::InvalidRequest),
            (500, FailureKind::Overloaded),
            (503, FailureKind::Overloaded),
            (504, FailureKind::Timeout),
            (529, FailureKind::Overloaded),
        ] {
            assert_eq!(
                classify(&provider_error(Some(status), "{}")),
                expected,
                "HTTP {status}"
            );
        }
    }

    #[test]
    fn typed_variants_are_read_before_any_text() {
        assert_eq!(
            classify(&GenerationError::Auth("bad".into())),
            FailureKind::Auth
        );
        assert_eq!(
            classify(&GenerationError::RateLimited { retry_after: None }),
            FailureKind::RateLimit
        );
        assert_eq!(
            classify(&GenerationError::Http("connection reset".into())),
            FailureKind::Network
        );
        assert_eq!(
            classify(&GenerationError::InvalidRequest("too long".into())),
            FailureKind::InvalidRequest
        );
        assert_eq!(
            classify(&GenerationError::InvalidResponse("garbled".into())),
            FailureKind::Other
        );
        assert_eq!(
            classify(&GenerationError::Refusal("no".into())),
            FailureKind::Other
        );
    }

    #[test]
    fn a_stream_error_event_is_classified_by_its_error_type() {
        for (text, expected) in [
            ("overloaded_error: Overloaded", FailureKind::Overloaded),
            ("api_error: Internal server error", FailureKind::Overloaded),
            ("rate_limit_error: slow down", FailureKind::RateLimit),
            ("authentication_error: bad key", FailureKind::Auth),
            ("invalid_request_error: nope", FailureKind::InvalidRequest),
            ("something_else_error: huh", FailureKind::Other),
            ("an ordinary sentence: with a colon", FailureKind::Other),
        ] {
            assert_eq!(classify(&provider_error(None, text)), expected, "{text}");
        }
    }

    #[test]
    fn bedrock_diagnostic_text_never_establishes_retryability() {
        for name in [
            "ModelTimeoutException",
            "ServiceUnavailableException",
            "InternalServerException",
            "ModelNotReadyException",
        ] {
            for text in [
                name.to_string(),
                format!("{name}: try later"),
                format!("ModelErrorException: invalid configuration referencing {name}"),
            ] {
                assert_eq!(
                    classify(&provider_error(None, &text)),
                    FailureKind::Other,
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn bedrock_operation_and_event_sources_classify_without_diagnostic_text() {
        use aws_sdk_bedrockruntime::operation::converse::ConverseError;
        use aws_sdk_bedrockruntime::operation::converse_stream::ConverseStreamError;
        use aws_sdk_bedrockruntime::types::error::{
            ConverseStreamOutputError, InternalServerException, ModelErrorException,
            ModelNotReadyException, ModelTimeoutException, ServiceUnavailableException,
        };

        let cases: Vec<(Box<dyn Error + Send + Sync>, FailureKind)> = vec![
            (
                Box::new(ConverseError::ModelTimeoutException(
                    ModelTimeoutException::builder().message("no discriminator here").build(),
                )),
                FailureKind::Timeout,
            ),
            (
                Box::new(ConverseError::ModelNotReadyException(
                    ModelNotReadyException::builder().build(),
                )),
                FailureKind::Overloaded,
            ),
            (
                Box::new(ConverseStreamError::ServiceUnavailableException(
                    ServiceUnavailableException::builder().build(),
                )),
                FailureKind::Overloaded,
            ),
            (
                Box::new(ConverseStreamOutputError::InternalServerException(
                    InternalServerException::builder().build(),
                )),
                FailureKind::Overloaded,
            ),
            (
                Box::new(ConverseError::ModelErrorException(
                    ModelErrorException::builder()
                        .message("ModelTimeoutException ServiceUnavailableException InternalServerException ModelNotReadyException")
                        .build(),
                )),
                FailureKind::Other,
            ),
            (
                Box::new(ConverseError::unhandled(std::io::Error::other("ModelTimeoutException"))),
                FailureKind::Other,
            ),
        ];
        for (source, expected) in cases {
            let error = GenerationError::Provider {
                status: None,
                message: "ModelTimeoutException: misleading diagnostic".into(),
                source: Some(source),
            };
            assert_eq!(classify(&error), expected, "{error:?}");
        }
    }

    #[test]
    fn a_status_outranks_the_error_type_in_the_body() {
        // A 400 whose body names `overloaded_error` is still the caller's
        // mistake, and a 529 whose body names nothing useful is still overload.
        assert_eq!(
            classify(&provider_error(Some(400), &body("overloaded_error", "x"))),
            FailureKind::InvalidRequest
        );
        assert_eq!(
            classify(&provider_error(Some(529), "not json at all")),
            FailureKind::Overloaded
        );
    }

    #[test]
    fn the_timeout_error_is_a_timeout_and_nothing_else_is() {
        let error = timed_out(Duration::from_secs(300));
        assert_eq!(classify(&error), FailureKind::Timeout);
        assert_eq!(
            timed_out_after(&error),
            Some(Duration::from_secs(300)),
            "the wait is recoverable from the error"
        );
        assert_eq!(timed_out_after(&provider_error(None, "x")), None);
    }

    #[test]
    fn transient_kinds_are_exactly_the_retryable_ones() {
        let transient: Vec<_> = [
            FailureKind::Auth,
            FailureKind::RateLimit,
            FailureKind::Overloaded,
            FailureKind::Timeout,
            FailureKind::Network,
            FailureKind::InvalidRequest,
            FailureKind::Other,
        ]
        .into_iter()
        .filter(|kind| kind.is_transient())
        .collect();
        assert_eq!(
            transient,
            [
                FailureKind::RateLimit,
                FailureKind::Overloaded,
                FailureKind::Timeout,
                FailureKind::Network
            ]
        );
    }

    #[test]
    fn labels_match_the_serialized_names() {
        for kind in [
            FailureKind::Auth,
            FailureKind::RateLimit,
            FailureKind::Overloaded,
            FailureKind::Timeout,
            FailureKind::Network,
            FailureKind::InvalidRequest,
            FailureKind::Other,
        ] {
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::Value::String(kind.label().to_string())
            );
        }
    }

    #[test]
    fn an_auth_failure_names_the_key_and_hides_the_raw_body() {
        let raw = body("authentication_error", "invalid x-api-key");
        let failure = describe(
            &provider_error(Some(401), &raw),
            ANTHROPIC,
            Attempts::default(),
        );
        assert_eq!(failure.kind, FailureKind::Auth);
        assert_eq!(
            failure.message,
            "Anthropic rejected the API key (HTTP 401, authentication_error). \
             Check the key saved by `polaris login anthropic` or supplied through ANTHROPIC_API_KEY."
        );
        assert!(!failure.message.contains("req_011"));
        assert!(!failure.message.contains('{'));
    }

    #[test]
    fn an_exhausted_overload_says_how_hard_it_tried() {
        let failure = describe(
            &provider_error(Some(529), &body("overloaded_error", "Overloaded")),
            ANTHROPIC,
            Attempts {
                retries: 4,
                elapsed: Duration::from_millis(37_200),
                committed: false,
            },
        );
        assert_eq!(failure.kind, FailureKind::Overloaded);
        assert_eq!(
            failure.message,
            "Anthropic is overloaded or unavailable (HTTP 529, overloaded_error). \
             Gave up after 4 retries over 38 s."
        );
    }

    #[test]
    fn a_failure_after_output_started_says_it_was_not_retried() {
        let failure = describe(
            &GenerationError::Http("connection reset".into()),
            ANTHROPIC,
            Attempts {
                committed: true,
                ..Attempts::default()
            },
        );
        assert_eq!(failure.kind, FailureKind::Network);
        assert!(
            failure
                .message
                .ends_with("The response had already started, so it was not retried."),
            "{}",
            failure.message
        );
        assert!(!failure.message.contains("Gave up"));
    }

    #[test]
    fn a_rate_limit_reports_the_wait_the_provider_asked_for() {
        let failure = describe(
            &GenerationError::RateLimited {
                retry_after: Some(Duration::from_secs(90)),
            },
            "aws.bedrock/anthropic.claude",
            Attempts::default(),
        );
        assert_eq!(
            failure.message,
            "Amazon Bedrock rate limited the request and asked for a wait of 90 s."
        );
    }

    #[test]
    fn bedrock_and_openai_hints_use_registered_provider_ids() {
        let bedrock = "aws.bedrock/global.anthropic.claude-sonnet-5";
        assert_eq!(
            describe(
                &GenerationError::Auth("denied".into()),
                bedrock,
                Attempts::default()
            )
            .message,
            "Amazon Bedrock rejected the AWS credentials. Check your AWS credentials and the model access granted to them."
        );
        assert_eq!(
            describe(
                &GenerationError::Http("reset".into()),
                bedrock,
                Attempts::default()
            )
            .message,
            "Could not reach Amazon Bedrock: reset. Check your AWS credentials, region and network connection."
        );
        assert_eq!(
            describe(
                &GenerationError::Auth("bad key".into()),
                "openai/gpt-5.5",
                Attempts::default()
            )
            .message,
            "OpenAI rejected the API key. Check the key saved by `polaris login openai` or supplied through OPENAI_API_KEY."
        );
    }

    #[test]
    fn a_gateway_timeout_without_a_local_timer_has_no_wait_duration() {
        assert_eq!(
            describe(
                &provider_error(Some(504), "gateway timeout"),
                ANTHROPIC,
                Attempts::default()
            )
            .message,
            "Anthropic timed out (HTTP 504)."
        );
    }

    #[test]
    fn unknown_errors_preserve_their_short_detail() {
        assert_eq!(
            describe(
                &GenerationError::InvalidResponse("garbled reply".into()),
                "custom/model",
                Attempts::default()
            )
            .message,
            "custom request failed: invalid response: garbled reply"
        );
        assert_eq!(
            classify(&provider_error(None, r#"{"error":{"code":123}}"#)),
            FailureKind::Other
        );
        assert_eq!(
            describe(
                &provider_error(None, r#"{"error":{"code":123}}"#),
                "model",
                Attempts::default()
            )
            .message,
            r#"The model provider request failed: {"error":{"code":123}}"#
        );
    }

    #[test]
    fn permanent_failures_after_output_do_not_claim_retry_was_suppressed() {
        assert_eq!(
            describe(
                &GenerationError::InvalidRequest("bad configuration".into()),
                ANTHROPIC,
                Attempts {
                    committed: true,
                    ..Attempts::default()
                }
            )
            .message,
            "Anthropic rejected the request: bad configuration"
        );
        assert_eq!(
            describe(
                &GenerationError::InvalidRequest("bad configuration".into()),
                ANTHROPIC,
                Attempts {
                    committed: true,
                    retries: 1,
                    elapsed: Duration::from_secs(2)
                },
            )
            .message,
            "Anthropic rejected the request: bad configuration Gave up after 1 retry over 2 s."
        );
    }

    #[test]
    fn a_timeout_names_how_long_it_waited() {
        let failure = describe(
            &timed_out(Duration::from_secs(300)),
            ANTHROPIC,
            Attempts {
                retries: 1,
                elapsed: Duration::from_secs(605),
                committed: false,
            },
        );
        assert_eq!(failure.kind, FailureKind::Timeout);
        assert_eq!(
            failure.message,
            "No response from Anthropic for 300 s. Gave up after 1 retry over 605 s."
        );
    }

    #[test]
    fn a_rejected_request_carries_the_providers_reason_on_one_line() {
        let failure = describe(
            &provider_error(
                Some(400),
                &body(
                    "invalid_request_error",
                    "prompt is too long:\n250000 tokens > 200000 maximum",
                ),
            ),
            ANTHROPIC,
            Attempts::default(),
        );
        assert_eq!(failure.kind, FailureKind::InvalidRequest);
        assert_eq!(
            failure.message,
            "Anthropic rejected the request (HTTP 400, invalid_request_error): \
             prompt is too long: 250000 tokens > 200000 maximum"
        );
    }

    #[test]
    fn an_unreachable_provider_names_the_cause_and_a_next_step() {
        let failure = describe(
            &GenerationError::Http(
                "error sending request for url (https://api.anthropic.com/v1/messages)".into(),
            ),
            ANTHROPIC,
            Attempts::default(),
        );
        assert_eq!(
            failure.message,
            "Could not reach Anthropic: error sending request for url \
             (https://api.anthropic.com/v1/messages). \
             Check your network connection and proxy settings."
        );
    }

    #[test]
    fn a_long_detail_is_cut() {
        let failure = describe(
            &GenerationError::InvalidResponse("x".repeat(1000)),
            ANTHROPIC,
            Attempts::default(),
        );
        assert!(failure.message.chars().count() < 300, "{}", failure.message);
        assert!(failure.message.contains('…'));
    }

    #[test]
    fn an_unknown_provider_still_gets_a_sentence() {
        let failure = describe(
            &GenerationError::Auth("denied".into()),
            "acme/model-1",
            Attempts::default(),
        );
        assert_eq!(
            failure.message,
            "acme rejected the credentials. Check the provider credentials."
        );
        let bare = describe(
            &GenerationError::Auth("denied".into()),
            "model-1",
            Attempts::default(),
        );
        assert!(bare.message.starts_with("The model provider rejected"));
    }

    #[test]
    fn a_plain_string_converts_to_an_other_failure() {
        let failure = TurnFailure::from("session busy");
        assert_eq!(failure.kind, FailureKind::Other);
        assert_eq!(failure.to_string(), "session busy");
    }
}
