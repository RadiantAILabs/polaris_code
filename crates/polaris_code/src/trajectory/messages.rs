//! Encode native messages using ATIF fields, with references instead of duplicate text.

use polaris::models::llm::{AssistantBlock, Message, ToolResultContent, UserBlock};
use serde_json::{Value, json};
use std::io;

pub(super) fn step(document: &mut Value, source: &str, kind: &str) -> usize {
    let steps = document["steps"].as_array_mut().expect("steps array");
    let id = steps.len() + 1;
    steps.push(json!({"step_id": id, "timestamp": super::timestamp(), "source": source, "message": "", "extra": {"kind": kind}}));
    id
}

fn refer(native: &mut Value, refs: &mut Vec<Value>, field: String, target: String) {
    *native.pointer_mut(&field).expect("native message field") = Value::Null;
    refs.push(json!({"native": field, "value": target}));
}

pub(super) fn tool_results(document: &mut Value, blocks: &[UserBlock]) -> bool {
    let mut changed = false;
    for block in blocks {
        let UserBlock::ToolResult(result) = block else {
            continue;
        };
        let steps = document["steps"].as_array_mut().expect("steps array");
        let Some(owner) = steps.iter_mut().rev().find(|step| {
            step["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|call| call["tool_call_id"] == result.id))
        }) else {
            continue;
        };
        if owner.get("observation").is_none() {
            owner["observation"] = json!({"results": []});
        }
        let observations = owner["observation"]["results"]
            .as_array_mut()
            .expect("observations");
        if observations
            .iter()
            .any(|item| item["source_call_id"] == result.id)
        {
            continue;
        }
        let mut observation = json!({"source_call_id": result.id, "extra": {"call_id": result.call_id, "status": result.status}});
        match &result.content {
            ToolResultContent::Text(text) => observation["content"] = json!(text),
            ToolResultContent::Image(image) => {
                observation["content"] = json!("[image]");
                observation["extra"]["image"] = json!(image);
            }
        }
        observations.push(observation);
        changed = true;
    }
    changed
}

fn observation_reference(document: &Value, id: &str, image: bool) -> Option<String> {
    let steps = document["steps"].as_array()?;
    for (step, value) in steps.iter().enumerate().rev() {
        if let Some(results) = value["observation"]["results"].as_array()
            && let Some(result) = results.iter().position(|item| item["source_call_id"] == id)
        {
            let field = if image { "extra/image" } else { "content" };
            return Some(format!(
                "/steps/{step}/observation/results/{result}/{field}"
            ));
        }
    }
    None
}

pub(super) fn store(document: &mut Value, id: usize, message: &Message) {
    if let Message::User { content } = message {
        tool_results(document, content);
    }
    let mut native = json!(message);
    let mut refs = Vec::new();
    let mut texts = Vec::new();
    let mut text_fields = Vec::new();
    let mut reasoning = String::new();
    let mut has_reasoning = false;
    let mut calls = Vec::new();
    match message {
        Message::User { content } => {
            for (index, block) in content.iter().enumerate() {
                match block {
                    UserBlock::Text(text) => {
                        texts.push(json!({"type": "text", "text": text.text}));
                        text_fields.push(format!("/content/{index}/text"));
                    }
                    UserBlock::ToolResult(result) => {
                        let image = matches!(&result.content, ToolResultContent::Image(_));
                        let target = observation_reference(document, &result.id, image);
                        let content = match &result.content {
                            ToolResultContent::Text(text) => json!(text),
                            ToolResultContent::Image(image) => json!(image),
                        };
                        if let Some(target) =
                            target.filter(|target| document.pointer(target) == Some(&content))
                        {
                            let variant = if image { "Image" } else { "Text" };
                            refer(
                                &mut native,
                                &mut refs,
                                format!("/content/{index}/content/{variant}"),
                                target,
                            );
                        } else if let ToolResultContent::Text(text) = &result.content {
                            // Elision rewrites an old observation for a new request.
                            // Keep the original evidence and record this version once.
                            texts.push(json!({"type": "text", "text": text}));
                            text_fields.push(format!("/content/{index}/content/Text"));
                        }
                    }
                    _ => {} // Native media/document data remains once in the skeleton.
                }
            }
        }
        Message::Assistant { content, .. } => {
            for (index, block) in content.iter().enumerate() {
                match block {
                    AssistantBlock::Text(text) => {
                        texts.push(json!({"type": "text", "text": text.text}));
                        text_fields.push(format!("/content/{index}/text"));
                    }
                    AssistantBlock::Reasoning(block) => {
                        has_reasoning = true;
                        for (part, text) in block.reasoning.iter().enumerate() {
                            let start = reasoning.len();
                            reasoning.push_str(text);
                            *native
                                .pointer_mut(&format!("/content/{index}/reasoning/{part}"))
                                .expect("reasoning") = Value::Null;
                            refs.push(json!({"native": format!("/content/{index}/reasoning/{part}"), "value": format!("/steps/{}/reasoning_content", id - 1), "start": start, "end": reasoning.len()}));
                        }
                    }
                    AssistantBlock::ToolCall(call) => {
                        let call_index = calls.len();
                        calls.push(super::tool_call(
                            &call.id,
                            &call.function.name,
                            call.function.arguments.clone(),
                        ));
                        let suffix = if call.function.arguments.is_object() {
                            ""
                        } else {
                            "/_native_arguments"
                        };
                        refer(
                            &mut native,
                            &mut refs,
                            format!("/content/{index}/function/arguments"),
                            format!(
                                "/steps/{}/tool_calls/{call_index}/arguments{suffix}",
                                id - 1
                            ),
                        );
                    }
                }
            }
        }
    }
    let text_count = texts.len();
    for (index, field) in text_fields.into_iter().enumerate() {
        let suffix = if text_count == 1 {
            String::new()
        } else {
            format!("/{index}/text")
        };
        refer(
            &mut native,
            &mut refs,
            field,
            format!("/steps/{}/message{suffix}", id - 1),
        );
    }
    let step = &mut document["steps"][id - 1];
    step["message"] = match text_count {
        0 => json!(""),
        1 => texts.remove(0)["text"].take(),
        _ => json!(texts),
    };
    for field in ["reasoning_content", "tool_calls"] {
        step.as_object_mut().unwrap().remove(field);
    }
    if has_reasoning {
        step["reasoning_content"] = json!(reasoning);
    }
    if !calls.is_empty() {
        step["tool_calls"] = json!(calls);
    }
    step["extra"]["native_message"] = native;
    step["extra"]["native_refs"] = json!(refs);
}

pub(super) fn load(document: &Value, id: usize) -> io::Result<Message> {
    let fail = || io::Error::other("invalid trajectory message reference");
    let step = document["steps"]
        .get(id.checked_sub(1).ok_or_else(fail)?)
        .ok_or_else(fail)?;
    let mut native = step["extra"]
        .get("native_message")
        .ok_or_else(fail)?
        .clone();
    for reference in step["extra"]["native_refs"].as_array().ok_or_else(fail)? {
        let pointer = reference["value"].as_str().ok_or_else(fail)?;
        let mut value = document.pointer(pointer).ok_or_else(fail)?.clone();
        if let Some(start) = reference.get("start") {
            let start = start.as_u64().ok_or_else(fail)? as usize;
            let end = reference["end"].as_u64().ok_or_else(fail)? as usize;
            value = json!(
                value
                    .as_str()
                    .and_then(|text| text.get(start..end))
                    .ok_or_else(fail)?
            );
        }
        *native
            .pointer_mut(reference["native"].as_str().ok_or_else(fail)?)
            .ok_or_else(fail)? = value;
    }
    serde_json::from_value(native).map_err(io::Error::other)
}
