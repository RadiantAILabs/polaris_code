//! Generic presentation contract for a host-owned rewind operation.
//!
//! The TUI does not interpret checkpoints or edit journals. A host supplies
//! already-decided axis outcomes and exclusions; this module only transports
//! and renders them without collapsing a partial result into “undone”.

use polaris::plugins::{IO_TYPE, IOMessage};

/// Metadata key carrying a serialized [`RewindReport`].
pub const META_REWIND_REPORT: &str = "rewind_report";
/// Presentation-only IO type for rewind reports.
pub const IO_TYPE_REWIND: &str = "rewind";

/// Host-decided outcome for one independent rewind axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindAxis {
    /// Axis name, such as `conversation`, `files`, `index`, or `persistence`.
    pub name: String,
    /// Truthful status, such as `restored`, `partial`, `refused`, or `not-requested`.
    pub status: String,
    /// Human-readable details, including individual conflicts where relevant.
    pub detail: String,
}

/// Complete presentation value for one operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindReport {
    /// Host operation/point identity.
    pub operation: String,
    /// Requested mode.
    pub mode: String,
    /// Independent outcomes; consumers must not reduce them to one success bit.
    pub axes: Vec<RewindAxis>,
    /// Effects explicitly outside the tracking boundary.
    pub exclusions: Vec<String>,
}

impl RewindReport {
    /// Build a generic report.
    #[must_use]
    pub fn new(operation: impl Into<String>, mode: impl Into<String>) -> Self {
        Self {
            operation: operation.into(),
            mode: mode.into(),
            axes: Vec::new(),
            exclusions: Vec::new(),
        }
    }

    /// Add an independent outcome.
    #[must_use]
    pub fn with_axis(
        mut self,
        name: impl Into<String>,
        status: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        self.axes.push(RewindAxis {
            name: name.into(),
            status: status.into(),
            detail: detail.into(),
        });
        self
    }

    /// Add one explicit exclusion.
    #[must_use]
    pub fn excluding(mut self, effect: impl Into<String>) -> Self {
        self.exclusions.push(effect.into());
        self
    }

    /// Render and stamp the structured value on an ordinary panel message.
    #[must_use]
    pub fn into_message(self) -> IOMessage {
        let mut text = format!("Rewind {} ({})", self.operation, self.mode);
        for axis in &self.axes {
            text.push_str(&format!(
                "\n\n{}: {}\n{}",
                axis.name, axis.status, axis.detail
            ));
        }
        if !self.exclusions.is_empty() {
            text.push_str(&format!(
                "\n\nOutside tracking: {}.",
                self.exclusions.join(", ")
            ));
        }
        let encoded = serde_json::json!({
            "operation": self.operation,
            "mode": self.mode,
            "axes": self.axes.iter().map(|axis| serde_json::json!({"name": axis.name, "status": axis.status, "detail": axis.detail})).collect::<Vec<_>>(),
            "exclusions": self.exclusions,
        }).to_string();
        IOMessage::system_text(text)
            .with_metadata(IO_TYPE, IO_TYPE_REWIND)
            .with_metadata(META_REWIND_REPORT, encoded)
    }
}

/// Parse a host report, returning `None` for unrelated or malformed messages.
#[must_use]
pub fn parse_rewind_report(message: &IOMessage) -> Option<RewindReport> {
    if message.metadata.get(IO_TYPE).map(String::as_str) != Some(IO_TYPE_REWIND) {
        return None;
    }
    let value: serde_json::Value =
        serde_json::from_str(message.metadata.get(META_REWIND_REPORT)?).ok()?;
    let axes = value
        .get("axes")?
        .as_array()?
        .iter()
        .map(|axis| {
            Some(RewindAxis {
                name: axis.get("name")?.as_str()?.to_string(),
                status: axis.get("status")?.as_str()?.to_string(),
                detail: axis.get("detail")?.as_str()?.to_string(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    let exclusions = value
        .get("exclusions")?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    Some(RewindReport {
        operation: value.get("operation")?.as_str()?.to_string(),
        mode: value.get("mode")?.as_str()?.to_string(),
        axes,
        exclusions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::plugins::IOContent;
    #[test]
    fn partial_axes_and_exclusions_round_trip_and_render() {
        let report = RewindReport::new("7", "both")
            .with_axis(
                "files",
                "partial",
                "restored a.rs; skipped b.rs: external edit",
            )
            .with_axis("conversation", "restored", "checkpoint_2")
            .excluding("shell commands");
        let message = report.clone().into_message();
        assert_eq!(parse_rewind_report(&message), Some(report));
        let IOContent::Text(text) = message.content else {
            panic!("text report")
        };
        assert!(text.contains("files: partial"));
        assert!(text.contains("Outside tracking: shell commands"));
    }
}
