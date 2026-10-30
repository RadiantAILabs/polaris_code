//! What `--list-models` writes to stdout.

use super::report::OutputFormat;
use crate::providers::ModelListing;

/// What `--list-models` writes to stdout.
///
/// A listing is not a turn: it starts nothing, spends nothing and produces no
/// assistant text, so it is its own report rather than a
/// [`RunReport`](super::RunReport) with empty fields. It lives beside the
/// other reports for the same reason they do — every byte the front end prints
/// is rendered inside [`exec`](super), so the rendering is covered by tests
/// rather than only by running the binary.
///
/// # Example
///
/// ```
/// use polaris::models::pricing::PriceStore;
/// use polaris_code::exec::{ModelsReport, OutputFormat};
/// use polaris_code::credentials::CredentialStore;
/// use polaris_code::providers::{ModelListing, process_env};
///
/// let listing = ModelListing::observe(&process_env, &CredentialStore::user(), &PriceStore::builtin());
/// let text = ModelsReport::new(listing).stdout_payload(OutputFormat::Text);
/// assert!(text.contains("--provider anthropic"));
/// ```
#[derive(Debug, Clone)]
pub struct ModelsReport {
    listing: ModelListing,
}

impl ModelsReport {
    /// Captures an observed listing for rendering.
    #[must_use]
    pub const fn new(listing: ModelListing) -> Self {
        Self { listing }
    }

    /// What the front end writes to stdout for `format`.
    ///
    /// [`OutputFormat::StreamJson`] renders the same single object as
    /// [`OutputFormat::Json`]: a listing is one answer, not a message stream,
    /// and a scripted consumer that asked for line-delimited JSON still gets
    /// one parseable line. The `schema` field is `polaris.models.v1`.
    #[must_use]
    pub fn stdout_payload(&self, format: OutputFormat) -> String {
        match format {
            OutputFormat::Text => self.listing.to_text(),
            OutputFormat::Json | OutputFormat::StreamJson => {
                serde_json::to_string(&serde_json::json!({
                    "schema": "polaris.models.v1",
                    "providers": self.listing.providers,
                }))
                .unwrap_or_else(|err| {
                    format!(r#"{{"error":"failed to serialize the model listing: {err}"}}"#)
                })
            }
        }
    }

    /// The process exit code for a listing: `0` — reading the declared models
    /// is not a turn and cannot fail one.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        0
    }
}

#[cfg(test)]
mod tests {
    use polaris::models::pricing::PriceStore;

    use super::*;
    use crate::credentials::CredentialStore;

    fn report() -> ModelsReport {
        let none = |_: &str| None;
        ModelsReport::new(ModelListing::observe(
            &none,
            &None::<CredentialStore>,
            &PriceStore::builtin(),
        ))
    }

    #[test]
    fn text_is_the_listing_text() {
        let report = report();
        let text = report.stdout_payload(OutputFormat::Text);
        assert!(text.starts_with("--provider anthropic"), "{text}");
        assert!(text.contains("anthropic/claude-sonnet-5-5"), "{text}");
        assert!(text.contains("openai/gpt-5.5"), "{text}");
    }

    #[test]
    fn json_and_stream_json_are_one_parseable_object() {
        let report = report();
        let json = report.stdout_payload(OutputFormat::Json);
        assert_eq!(json, report.stdout_payload(OutputFormat::StreamJson));
        assert!(!json.contains('\n'), "one line: {json}");
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        assert_eq!(value["schema"], "polaris.models.v1");
        assert_eq!(value["providers"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn a_listing_always_exits_zero() {
        assert_eq!(report().exit_code(), 0);
    }
}
