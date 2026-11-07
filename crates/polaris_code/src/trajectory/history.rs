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

pub(super) fn resolve(document: &Value, reference: usize) -> io::Result<Vec<usize>> {
    if reference == 0 {
        return Ok(Vec::new());
    }
    let histories = document["extra"]["histories"]
        .as_array()
        .ok_or_else(invalid)?;
    let length = histories
        .get(reference - 1)
        .and_then(|node| node["length"].as_u64())
        .ok_or_else(invalid)? as usize;
    // Expand ranges iteratively: long runs must not consume one stack frame per call.
    let mut pending = vec![(reference, 0usize, length)];
    let mut result = Vec::new();
    while let Some((id, start, length)) = pending.pop() {
        if id == 0 {
            result.push(start);
            continue;
        } // a direct message step
        let node = histories.get(id - 1).ok_or_else(invalid)?;
        let total = node["length"].as_u64().ok_or_else(invalid)? as usize;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= total)
            .ok_or_else(invalid)?;
        let mut offset = 0usize;
        let mut selected = Vec::new();
        for segment in node["segments"].as_array().ok_or_else(invalid)? {
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
