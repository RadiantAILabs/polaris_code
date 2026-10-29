//! Best-effort summaries of externally graded gate-plan history.
//!
//! The evaluation harness appends one JSON object per officially graded
//! episode when [`PLAN_HISTORY_VAR`] names a file. This module deliberately
//! treats that file as advisory prompt context: a missing, unreadable, empty,
//! or partly malformed history never prevents a new plan from being made.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::Path;

use serde::Deserialize;

/// Environment variable naming the optional JSONL plan-history file.
pub const PLAN_HISTORY_VAR: &str = "POLARIS_CODE_PLAN_HISTORY";

/// Maximum number of shape summaries included in planner context.
const MAX_HISTORY_LINES: usize = 10;

/// Reads the optional plan-history file and renders its most-used shapes.
///
/// Returns `None` when [`PLAN_HISTORY_VAR`] is absent, the file cannot be
/// read, it is empty, or none of its lines contain a complete plan-shape
/// record. Individual malformed JSONL lines are ignored.
#[must_use]
pub fn render_plan_history() -> Option<String> {
    let path = env::var_os(PLAN_HISTORY_VAR)?;
    render_plan_history_at(Path::new(&path))
}

fn render_plan_history_at(path: &Path) -> Option<String> {
    let source = fs::read_to_string(path).ok()?;
    let mut shapes: BTreeMap<String, ShapeHistory> = BTreeMap::new();

    for line in source.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(record) = serde_json::from_str::<HistoryRecord>(line) else {
            continue;
        };
        let (Some(digest), Some(shape), Some(resolved)) =
            (record.shape_digest, record.shape, record.resolved)
        else {
            continue;
        };
        if digest.trim().is_empty() || shape.trim().is_empty() {
            continue;
        }
        let entry = shapes.entry(digest).or_insert_with(|| ShapeHistory {
            shape,
            used: 0,
            resolved: 0,
        });
        entry.used += 1;
        entry.resolved += usize::from(resolved);
    }

    let mut summaries: Vec<_> = shapes.into_values().collect();
    summaries.sort_by(|left, right| {
        right
            .used
            .cmp(&left.used)
            .then_with(|| left.shape.cmp(&right.shape))
    });
    let rendered: Vec<_> = summaries
        .into_iter()
        .take(MAX_HISTORY_LINES)
        .map(|summary| {
            format!(
                "{}: used {}, resolved {}",
                summary.shape, summary.used, summary.resolved
            )
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join("\n"))
}

#[derive(Deserialize)]
struct HistoryRecord {
    shape_digest: Option<String>,
    shape: Option<String>,
    resolved: Option<bool>,
}

struct ShapeHistory {
    shape: String,
    used: usize,
    resolved: usize,
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::render_plan_history_at;

    #[test]
    fn aggregates_shapes_by_digest_and_orders_by_use() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("history.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"shape_digest":"a","shape":"Seq(model-propose)","resolved":true}"#,
                "\n",
                r#"{"shape_digest":"b","shape":"Loop(apply-patch-intent)","resolved":false}"#,
                "\n",
                r#"{"shape_digest":"a","shape":"Seq(model-propose)","resolved":false}"#,
                "\n",
            ),
        )
        .expect("write history");

        assert_eq!(
            render_plan_history_at(&path),
            Some(
                "Seq(model-propose): used 2, resolved 1\nLoop(apply-patch-intent): used 1, resolved 0"
                    .to_owned()
            )
        );
    }

    #[test]
    fn caps_the_rendered_history() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("history.jsonl");
        let lines: String = (0..11)
            .map(|index| {
                format!(
                    "{{\"shape_digest\":\"{index}\",\"shape\":\"Seq(step-{index})\",\"resolved\":false}}\n"
                )
            })
            .collect();
        fs::write(&path, lines).expect("write history");

        let rendered = render_plan_history_at(&path).expect("rendered history");
        assert_eq!(rendered.lines().count(), 10);
        assert!(!rendered.contains("Seq(step-9)"));
    }

    #[test]
    fn skips_malformed_or_incomplete_lines() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("history.jsonl");
        fs::write(
            &path,
            concat!(
                "not json\n",
                r#"{"shape_digest":"a","shape":"Seq(ok)","resolved":true}"#,
                "\n",
                r#"{"shape_digest":"b","shape":"Seq(missing-outcome)"}"#,
                "\n",
                r#"{"shape_digest":"","shape":"Seq(empty)","resolved":false}"#,
                "\n",
            ),
        )
        .expect("write history");

        assert_eq!(
            render_plan_history_at(&path),
            Some("Seq(ok): used 1, resolved 1".to_owned())
        );
    }

    #[test]
    fn missing_or_empty_history_is_absent() {
        let directory = tempdir().expect("temporary directory");
        assert_eq!(
            render_plan_history_at(&directory.path().join("missing.jsonl")),
            None
        );
        let empty = directory.path().join("empty.jsonl");
        fs::write(&empty, "\n").expect("write empty history");
        assert_eq!(render_plan_history_at(&empty), None);
    }
}
