//! Persistent ordered message histories, sharing unchanged ranges across calls.

use std::collections::HashMap;
use std::io;

use serde_json::{Value, json};

#[derive(Default)]
pub(super) struct History {
    previous: Vec<usize>,
    reference: usize,
}

impl History {
    pub(super) fn record(&mut self, document: &mut Value, messages: Vec<usize>) -> usize {
        if messages == self.previous {
            return self.reference;
        }
        let mut positions: HashMap<usize, Vec<usize>> = HashMap::new();
        for (index, step) in self.previous.iter().enumerate() {
            positions.entry(*step).or_default().push(index);
        }
        let mut segments = Vec::new();
        let mut inserted = Vec::new();
        let mut index = 0;
        while index < messages.len() {
            let best = positions
                .get(&messages[index])
                .into_iter()
                .flatten()
                .map(|start| {
                    let length = self.previous[*start..]
                        .iter()
                        .zip(&messages[index..])
                        .take_while(|(old, new)| old == new)
                        .count();
                    (*start, length)
                })
                .max_by_key(|(_, length)| *length);
            if let Some((start, length)) = best.filter(|(_, length)| *length > 1) {
                if !inserted.is_empty() {
                    segments.push(json!({"steps": std::mem::take(&mut inserted)}));
                }
                segments.push(json!({"history": self.reference, "start": start, "length": length}));
                index += length;
            } else {
                inserted.push(messages[index]);
                index += 1;
            }
        }
        if !inserted.is_empty() {
            segments.push(json!({"steps": inserted}));
        }
        let histories = document["extra"]["histories"]
            .as_array_mut()
            .expect("histories array");
        histories.push(json!({"length": messages.len(), "segments": segments}));
        self.reference = histories.len();
        self.previous = messages;
        self.reference
    }
}

fn invalid() -> io::Error {
    io::Error::other("invalid trajectory history reference")
}

// These per-request limits allow one million messages, ten million traversal
// operations and 256 MiB of native message data, far above a real model context.
// Count shared ranges each time they expand, rather than just counting JSON nodes.
const MAX_MESSAGES: usize = 1_000_000;
const MAX_WORK: usize = 10_000_000;
const MAX_BYTES: usize = 256 * 1024 * 1024;

fn work(remaining: &mut usize) -> io::Result<()> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or_else(|| io::Error::other("trajectory history work budget exceeded"))?;
    Ok(())
}

pub(super) fn resolve(document: &Value, reference: usize) -> io::Result<Vec<usize>> {
    resolve_with_limits(document, reference, MAX_MESSAGES, MAX_WORK, MAX_BYTES)
}

fn resolve_with_limits(
    document: &Value,
    reference: usize,
    max_messages: usize,
    mut remaining_work: usize,
    mut remaining_bytes: usize,
) -> io::Result<Vec<usize>> {
    if reference == 0 {
        return Ok(Vec::new());
    }
    let histories = document["extra"]["histories"]
        .as_array()
        .ok_or_else(invalid)?;
    let length = histories
        .get(reference - 1)
        .and_then(|node| node["length"].as_u64())
        .ok_or_else(invalid)?;
    let bounded_length = |length: u64| -> io::Result<usize> {
        usize::try_from(length)
            .ok()
            .filter(|length| *length <= max_messages)
            .ok_or_else(|| io::Error::other("trajectory history length budget exceeded"))
    };
    let length = bounded_length(length)?;
    // Expand ranges iteratively: long runs must not consume one stack frame per call.
    let mut pending = vec![(reference, 0usize, length)];
    let mut result = Vec::new();
    while let Some((id, start, length)) = pending.pop() {
        work(&mut remaining_work)?;
        if id == 0 {
            if result.len() >= max_messages {
                return Err(io::Error::other(
                    "trajectory history length budget exceeded",
                ));
            }
            // Check cumulative clone size before rebuild_request loads any messages.
            super::messages::check_size(document, start, &mut remaining_bytes)?;
            result.push(start);
            continue;
        } // a direct message step
        let node = histories.get(id - 1).ok_or_else(invalid)?;
        let total = bounded_length(node["length"].as_u64().ok_or_else(invalid)?)?;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= total)
            .ok_or_else(invalid)?;
        let mut offset = 0usize;
        let mut selected = Vec::new();
        for segment in node["segments"].as_array().ok_or_else(invalid)? {
            work(&mut remaining_work)?;
            let count = segment
                .get("steps")
                .and_then(Value::as_array)
                .map_or_else(
                    || segment["length"].as_u64().map(|value| value as usize),
                    |steps| Some(steps.len()),
                )
                .ok_or_else(invalid)?;
            let segment_end = offset.checked_add(count).ok_or_else(invalid)?;
            let from = start.max(offset);
            let through = end.min(segment_end);
            if from < through {
                if let Some(steps) = segment.get("steps").and_then(Value::as_array) {
                    for step in &steps[from - offset..through - offset] {
                        work(&mut remaining_work)?;
                        selected.push((0, step.as_u64().ok_or_else(invalid)? as usize, 1));
                    }
                } else {
                    let parent = segment["history"].as_u64().ok_or_else(invalid)? as usize;
                    if parent == 0 || parent >= id {
                        return Err(invalid());
                    }
                    let base = segment["start"].as_u64().ok_or_else(invalid)? as usize;
                    selected.push((
                        parent,
                        base.checked_add(from - offset).ok_or_else(invalid)?,
                        through - from,
                    ));
                }
            }
            offset = segment_end;
        }
        if offset != total {
            return Err(invalid());
        }
        pending.extend(selected.into_iter().rev());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trajectory::{RunTrajectory, rebuild_request};
    use polaris::models::llm::{LlmRequest, Message};

    fn fixture() -> (Value, usize, usize) {
        let directory = tempfile::tempdir().unwrap();
        let trace = RunTrajectory::create(
            &directory.path().join("trajectory.json"),
            "run",
            "fake/model",
            "α-task",
        )
        .unwrap();
        let call = trace.begin(
            "fake/model",
            &LlmRequest {
                messages: vec![Message::user("α-task")],
                ..Default::default()
            },
        );
        let document = trace.0.recording.lock().unwrap().document.clone();
        let message = document["steps"]
            .as_array()
            .unwrap()
            .iter()
            .position(|step| step["extra"].get("native_message").is_some())
            .unwrap();
        (document, call + 1, message + 1)
    }

    #[test]
    fn tampered_documents_report_reference_errors() {
        let (original, call, message) = fixture();
        let history = original["steps"][call - 1]["extra"]["input"]["history"]
            .as_u64()
            .unwrap() as usize;
        let node = format!("/extra/histories/{}", history - 1);
        let extra = format!("/steps/{}/extra", message - 1);
        let invalid_call = "invalid model-call step";
        let invalid_history = "invalid trajectory history reference";
        let invalid_message = "invalid trajectory message reference";
        let cases = [
            ("step zero", "".into(), Value::Null, 0, invalid_call),
            (
                "step out of range",
                "".into(),
                Value::Null,
                usize::MAX,
                invalid_call,
            ),
            (
                "non-model-call step",
                "".into(),
                Value::Null,
                message,
                invalid_call,
            ),
            (
                "missing history",
                "/extra/histories".into(),
                json!([]),
                call,
                invalid_history,
            ),
            (
                "bad history length",
                format!("{node}/length"),
                json!("one"),
                call,
                invalid_history,
            ),
            (
                "missing segments",
                format!("{node}/segments"),
                Value::Null,
                call,
                invalid_history,
            ),
            (
                "offset differs from total",
                format!("{node}/length"),
                json!(2),
                call,
                invalid_history,
            ),
            (
                "bad direct step",
                format!("{node}/segments"),
                json!([{"steps": ["one"]}]),
                call,
                invalid_history,
            ),
            (
                "forward parent",
                format!("{node}/segments"),
                json!([{"history": history, "start": 0, "length": 1}]),
                call,
                invalid_history,
            ),
            (
                "zero parent",
                format!("{node}/segments"),
                json!([{"history": 0, "start": 0, "length": 1}]),
                call,
                invalid_history,
            ),
            (
                "zero message step",
                format!("{node}/segments"),
                json!([{"steps": [0]}]),
                call,
                invalid_message,
            ),
            (
                "missing message step",
                format!("{node}/segments"),
                json!([{"steps": [9999]}]),
                call,
                invalid_message,
            ),
            (
                "missing native message",
                extra.clone(),
                json!({"native_refs": []}),
                call,
                invalid_message,
            ),
            (
                "missing native refs",
                format!("{extra}/native_refs"),
                Value::Null,
                call,
                invalid_message,
            ),
            (
                "bad source pointer",
                format!("{extra}/native_refs/0/value"),
                json!("/absent"),
                call,
                invalid_message,
            ),
            (
                "bad native pointer",
                format!("{extra}/native_refs/0/native"),
                json!("/absent"),
                call,
                invalid_message,
            ),
            (
                "bad UTF-8 range",
                format!("{extra}/native_refs/0"),
                json!({"native": "/content/0/text", "value": format!("/steps/{}/message", message - 1), "start": 1, "end": 2}),
                call,
                invalid_message,
            ),
        ];
        for (name, pointer, value, step, expected) in cases {
            let mut document = original.clone();
            if !pointer.is_empty() {
                *document.pointer_mut(&pointer).unwrap() = value;
            }
            let error = rebuild_request(&document, step).unwrap_err();
            assert_eq!(error.to_string(), expected, "{name}");
        }

        // A valid backward edge can still request a range past its parent's end.
        for (name, start) in [("range past total", 1), ("range overflow", usize::MAX)] {
            let mut document = original.clone();
            document["extra"]["histories"]
                .as_array_mut()
                .unwrap()
                .push(json!({
                    "length": 1,
                    "segments": [{"history": history, "start": start, "length": 1}],
                }));
            let error = resolve(&document, history + 1).unwrap_err();
            assert_eq!(error.to_string(), invalid_history, "{name}");
        }
    }

    #[test]
    fn duplicated_parent_ranges_exhaust_length_and_work_budgets() {
        let (mut document, _, _) = fixture();
        let histories = document["extra"]["histories"].as_array_mut().unwrap();
        for parent in 1..=20 {
            let length = 1usize << (parent - 1);
            histories.push(json!({
                "length": length * 2,
                "segments": [
                    {"history": parent, "start": 0, "length": length},
                    {"history": parent, "start": 0, "length": length},
                ],
            }));
        }
        assert_eq!(
            resolve(&document, 21).unwrap_err().to_string(),
            "trajectory history length budget exceeded"
        );
        // Exercise traversal exhaustion with a smaller expansion below the length cap.
        assert_eq!(
            resolve_with_limits(&document, 11, MAX_MESSAGES, 100, MAX_BYTES)
                .unwrap_err()
                .to_string(),
            "trajectory history work budget exceeded"
        );
    }

    #[test]
    fn repeated_messages_count_toward_cumulative_byte_budget() {
        let (mut document, _, message) = fixture();
        let mut remaining = MAX_BYTES;
        super::super::messages::check_size(&document, message, &mut remaining).unwrap();
        let size = MAX_BYTES - remaining;
        document["extra"]["histories"] = json!([
            {"length": 2, "segments": [{"steps": [message, message]}]},
        ]);
        assert_eq!(
            resolve_with_limits(&document, 1, MAX_MESSAGES, MAX_WORK, size * 2 - 1)
                .unwrap_err()
                .to_string(),
            "trajectory message byte budget exceeded"
        );
        assert_eq!(
            resolve_with_limits(&document, 1, MAX_MESSAGES, MAX_WORK, size * 2).unwrap(),
            [message, message]
        );
    }

    #[test]
    fn ten_thousand_call_history_rebuilds_without_recursion() {
        let (mut document, call, message) = fixture();
        let histories = document["extra"]["histories"].as_array_mut().unwrap();
        for parent in 1..10_000 {
            histories.push(json!({
                "length": parent + 1,
                "segments": [
                    {"history": parent, "start": 0, "length": parent},
                    {"steps": [message]},
                ],
            }));
        }
        document["steps"][call - 1]["extra"]["input"]["history"] = json!(10_000);
        let rebuilt = rebuild_request(&document, call).unwrap();
        assert_eq!(rebuilt.messages.len(), 10_000);
        assert!(
            rebuilt
                .messages
                .iter()
                .all(|message| json!(message) == json!(Message::user("α-task")))
        );
    }
}
