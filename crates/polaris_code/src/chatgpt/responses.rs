//! Responses API request encoding and streamed response decoding for
//! [`ChatGptProvider`](super::ChatGptProvider).

use std::collections::{BTreeMap, VecDeque};

use futures_util::StreamExt;
use polaris::models::llm::{
    AssistantBlock, ContentBlockDelta, ContentBlockStartData, DocumentMediaType, DocumentSource,
    GenerationError, ImageMediaType, LlmRequest, LlmStream, Message, StopReason, StreamEvent,
    ToolChoice, ToolResultContent, Usage, UserBlock,
};
use serde_json::{Value, json};

use super::http::{bounded_json, server_detail};

/// Prefix of every reasoning signature this provider issues.
///
/// Encrypted reasoning is tagged with its account so it cannot be replayed to
/// another; the tag is removed before the reasoning is sent back to `OpenAI`.
pub(crate) const REASONING_PREFIX: &str = "polaris-code:responses:";

/// Namespace that groups Polaris tools in requests.
const TOOL_NAMESPACE: &str = "polaris";
/// Description of [`TOOL_NAMESPACE`] shown to the model.
const TOOL_NAMESPACE_DESCRIPTION: &str = "Polaris Code tools";

/// The reasoning signature prefix for one account registration.
pub(crate) fn reasoning_scope(account_id: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let mut hash = Sha256::new();
    hash.update((account_id.len() as u64).to_be_bytes());
    hash.update(account_id.as_bytes());
    let mut scope = format!("{REASONING_PREFIX}chatgpt:");
    for byte in hash.finalize() {
        write!(&mut scope, "{byte:02x}").expect("formatting into a String is infallible");
    }
    scope.push(':');
    scope
}

/// Whether `signature` is encrypted reasoning issued by a `ChatGPT` account.
pub(crate) fn is_chatgpt_reasoning(signature: &str) -> bool {
    signature.starts_with(REASONING_PREFIX)
}

pub(crate) fn request_body(
    model: &str,
    request: &LlmRequest,
    scope: &str,
) -> Result<Value, GenerationError> {
    let mut input = Vec::new();
    for message in &request.messages {
        match message {
            Message::User { content } => {
                let mut parts = Vec::new();
                for block in content {
                    match block {
                        UserBlock::Text(text) => {
                            parts.push(json!({"type":"input_text", "text":text.text}));
                        }
                        UserBlock::Image(image) => {
                            let DocumentSource::Base64(data) = &image.data;
                            let media = match image.media_type {
                                ImageMediaType::PNG => "image/png",
                                ImageMediaType::JPEG => "image/jpeg",
                                ImageMediaType::GIF => "image/gif",
                                ImageMediaType::WEBP => "image/webp",
                                _ => {
                                    return Err(GenerationError::UnsupportedContent(
                                        "convert this image to PNG, JPEG, GIF, or WebP before sending"
                                            .into(),
                                    ));
                                }
                            };
                            parts.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media};base64,{data}"),
                            }));
                        }
                        UserBlock::Document(document) => {
                            let DocumentSource::Base64(data) = &document.data;
                            let media = document_mime(&document.media_type);
                            parts.push(json!({
                                "type": "input_file",
                                "filename": document.name,
                                "file_data": format!("data:{media};base64,{data}"),
                            }));
                        }
                        UserBlock::ToolResult(result) => {
                            flush_user(&mut input, &mut parts);
                            let output = match &result.content {
                                ToolResultContent::Text(text) => json!(text),
                                ToolResultContent::Image(_) => {
                                    return Err(GenerationError::UnsupportedContent(
                                        "image tool results are not supported by the ChatGPT connection".into(),
                                    ));
                                }
                            };
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": required_call_id(result.call_id.as_deref(), "tool result")?,
                                "output": output,
                            }));
                        }
                        UserBlock::Audio(_) => {
                            return Err(GenerationError::UnsupportedContent(
                                "audio is unavailable for the ChatGPT connection".into(),
                            ));
                        }
                    }
                }
                flush_user(&mut input, &mut parts);
            }
            Message::Assistant { content, .. } => {
                for block in content {
                    match block {
                        AssistantBlock::Text(text) => {
                            input.push(json!({
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text.text}],
                            }));
                        }
                        AssistantBlock::ToolCall(call) => {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": required_call_id(call.call_id.as_deref(), "tool call")?,
                                "name": call.function.name,
                                "namespace": TOOL_NAMESPACE,
                                "arguments": call.function.arguments.to_string(),
                            }));
                        }
                        AssistantBlock::Reasoning(reasoning) => {
                            if let Some(signature) = reasoning
                                .signature
                                .as_deref()
                                .and_then(|signature| signature.strip_prefix(scope))
                            {
                                input.push(json!({
                                    "type": "reasoning",
                                    "encrypted_content": signature,
                                    "summary": [],
                                }));
                            }
                        }
                    }
                }
            }
        }
    }

    // Plan-backed requests must be stateless and streamed, and must omit
    // `max_output_tokens`, `temperature`, `top_p` and `metadata`, among others:
    // https://developers.openai.com/siwc/token-sharing-open-source/preview-limitations
    // A reply cut off at the backend's own limit arrives as `response.incomplete`,
    // which the agent continues from.
    let mut body = json!({"model":model, "input":input, "store":false, "stream":true});
    if let Some(system) = &request.system {
        body["instructions"] = json!(system);
    }
    if let Some(tools) = &request.tools
        && !tools.is_empty()
    {
        // Strict mode would require every schema field, including optional tool arguments.
        let tools: Vec<_> = tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                    "strict": false,
                })
            })
            .collect();
        body["tools"] = json!([{
            "type": "namespace",
            "name": TOOL_NAMESPACE,
            "description": TOOL_NAMESPACE_DESCRIPTION,
            "tools": tools,
        }]);
    }
    if let Some(choice) = &request.tool_choice {
        body["tool_choice"] = match choice {
            ToolChoice::Auto => json!("auto"),
            ToolChoice::None => json!("none"),
            ToolChoice::Required => json!("required"),
            ToolChoice::Specific(name) => {
                json!({"type":"function", "name":name, "namespace":TOOL_NAMESPACE})
            }
        };
    }
    if let Some(schema) = &request.output_schema {
        // Not strict, for the same reason as tools: caller schemas may have optional fields.
        body["text"] = json!({"format":{"type":"json_schema", "name":"structured_output", "schema":schema, "strict":false}});
    }
    if let Some(effort) = &request.reasoning_effort {
        body["reasoning"] = json!({"effort": effort.as_str()});
    }
    body["include"] = json!(["reasoning.encrypted_content"]);
    Ok(body)
}

/// The Responses `call_id` linking a tool call to its result. The provider's
/// own item id is not interchangeable with it, so a missing one is refused.
fn required_call_id<'a>(call_id: Option<&'a str>, what: &str) -> Result<&'a str, GenerationError> {
    call_id.filter(|id| !id.is_empty()).ok_or_else(|| {
        GenerationError::InvalidRequest(format!(
            "{what} has no Responses call_id; it cannot be sent to ChatGPT"
        ))
    })
}

fn document_mime(media_type: &DocumentMediaType) -> &'static str {
    match media_type {
        DocumentMediaType::PDF => "application/pdf",
        DocumentMediaType::TXT => "text/plain",
        DocumentMediaType::HTML => "text/html",
        DocumentMediaType::MARKDOWN => "text/markdown",
        DocumentMediaType::CSV => "text/csv",
    }
}

fn flush_user(input: &mut Vec<Value>, parts: &mut Vec<Value>) {
    if !parts.is_empty() {
        input.push(json!({"role":"user", "content":std::mem::take(parts)}));
    }
}

const MAX_EVENT_BYTES: usize = 1024 * 1024;
const MAX_STREAM_BYTES: usize = 64 * 1024 * 1024;

/// Byte limits enforced on the raw stream before the SSE parser buffers it.
struct StreamBounds {
    event_bytes: usize,
    total_bytes: usize,
    line_empty: bool,
    failed: bool,
}

impl Default for StreamBounds {
    fn default() -> Self {
        Self {
            event_bytes: 0,
            total_bytes: 0,
            line_empty: true,
            failed: false,
        }
    }
}

impl StreamBounds {
    fn check(&mut self, bytes: &[u8]) -> Result<(), GenerationError> {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        if self.total_bytes > MAX_STREAM_BYTES {
            return Err(GenerationError::InvalidResponse(
                "the response stream exceeds 64 MiB".into(),
            ));
        }
        for byte in bytes {
            self.event_bytes += 1;
            if self.event_bytes > MAX_EVENT_BYTES {
                return Err(GenerationError::InvalidResponse(
                    "a response stream event exceeds 1 MiB".into(),
                ));
            }
            if *byte == b'\n' {
                if self.line_empty {
                    self.event_bytes = 0;
                }
                self.line_empty = true;
            } else if *byte != b'\r' {
                self.line_empty = false;
            }
        }
        Ok(())
    }

    /// Pass `chunk` through if it is within bounds. Ends the stream after the
    /// first error.
    fn admit<B: AsRef<[u8]>>(
        &mut self,
        chunk: reqwest::Result<B>,
    ) -> Option<Result<B, GenerationError>> {
        if self.failed {
            return None;
        }
        let result = chunk
            .map_err(|_| GenerationError::Http("the response stream was interrupted".into()))
            .and_then(|chunk| self.check(chunk.as_ref()).map(|()| chunk));
        self.failed = result.is_err();
        Some(result)
    }
}

/// Map a non-success `/responses` status and body to a [`GenerationError`].
fn status_error(
    status: u16,
    retry_after: Option<std::time::Duration>,
    body: &Value,
) -> GenerationError {
    let code = body["error"]["code"].as_str().unwrap_or("");
    if code.starts_with("subscription_sharing_") || code.starts_with("chatpass_") {
        return service_error(code);
    }
    let with_detail = |message: String| match server_detail(body) {
        Some(detail) => format!("{message}: {detail}"),
        None => message,
    };
    match status {
        401 | 403 => GenerationError::Auth(with_detail(format!(
            "the model service rejected this connection's credentials or permissions (HTTP {status})"
        ))),
        429 => GenerationError::RateLimited { retry_after },
        408 | 500..=599 => GenerationError::Provider {
            status: Some(status),
            message: "the model service is temporarily unavailable".into(),
            source: None,
        },
        _ => GenerationError::InvalidRequest(with_detail(format!(
            "the model service rejected the request (HTTP {status})"
        ))),
    }
}

/// Decode a `/responses` reply into a stream of events.
///
/// The stream ends after `response.completed` or a handled
/// `response.incomplete`, and yields an error if the body ends before either.
pub(crate) async fn response_stream(
    response: reqwest::Response,
    scope: String,
) -> Result<LlmStream, GenerationError> {
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);
        let body: Value = bounded_json(response).await.unwrap_or_default();
        return Err(status_error(status, retry_after, &body));
    }
    let bytes = response
        .bytes_stream()
        .scan(StreamBounds::default(), |bounds, chunk| {
            futures_util::future::ready(bounds.admit(chunk))
        });
    let events = EventStream {
        raw: sse_stream::SseStream::from_bytes_stream(bytes).boxed(),
        decoder: Decoder {
            scope,
            ..Decoder::default()
        },
        queue: VecDeque::new(),
        ended: false,
    };
    Ok(Box::pin(futures_util::stream::unfold(
        events,
        |mut events| async move { events.next_event().await.map(|event| (event, events)) },
    )))
}

type SseEvents =
    futures_util::stream::BoxStream<'static, Result<sse_stream::Sse, sse_stream::Error>>;

/// Decoded events not yet yielded, and the SSE source they come from.
struct EventStream {
    raw: SseEvents,
    decoder: Decoder,
    queue: VecDeque<Result<StreamEvent, GenerationError>>,
    ended: bool,
}

impl EventStream {
    async fn next_event(&mut self) -> Option<Result<StreamEvent, GenerationError>> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some(event);
            }
            if self.ended {
                return None;
            }
            match self.raw.next().await {
                Some(Ok(event)) => {
                    let Some(data) = event.data else {
                        continue;
                    };
                    let decoded = serde_json::from_str::<Value>(&data)
                        .map_err(|_| {
                            GenerationError::InvalidResponse(
                                "invalid Responses stream event".into(),
                            )
                        })
                        .and_then(|event| self.decoder.event(&event));
                    match decoded {
                        Ok(events) => {
                            self.ended = self.decoder.completed;
                            self.queue.extend(events.into_iter().map(Ok));
                        }
                        Err(error) => self.fail(error),
                    }
                }
                Some(Err(sse_stream::Error::Body(source))) => {
                    self.fail(match source.downcast::<GenerationError>() {
                        Ok(error) => *error,
                        Err(_) => {
                            GenerationError::Http("the response stream was interrupted".into())
                        }
                    });
                }
                Some(Err(_)) => self.fail(GenerationError::InvalidResponse(
                    "the response stream contains an invalid SSE event".into(),
                )),
                None => self.fail(GenerationError::InvalidResponse(
                    "the response ended before response.completed; this turn is incomplete".into(),
                )),
            }
        }
    }

    fn fail(&mut self, error: GenerationError) {
        self.ended = true;
        self.queue.push_back(Err(error));
    }
}

#[derive(Default)]
struct Decoder {
    scope: String,
    blocks: BTreeMap<(u64, u64), u32>,
    next: u32,
    tools: bool,
    refusal: Option<String>,
    completed: bool,
}

impl Decoder {
    fn start(
        &mut self,
        key: (u64, u64),
        block: ContentBlockStartData,
        events: &mut Vec<StreamEvent>,
    ) -> u32 {
        if let Some(index) = self.blocks.get(&key) {
            return *index;
        }
        let index = self.next;
        self.next += 1;
        self.blocks.insert(key, index);
        events.push(StreamEvent::ContentBlockStart { index, block });
        index
    }

    fn event(&mut self, event: &Value) -> Result<Vec<StreamEvent>, GenerationError> {
        let mut events = Vec::new();
        let output = event["output_index"].as_u64().unwrap_or(0);
        let content = event["content_index"].as_u64().unwrap_or(0);
        match event["type"].as_str().unwrap_or("") {
            "response.output_item.added" if event["item"]["type"] == "function_call" => {
                self.tools = true;
                let item = &event["item"];
                self.start(
                    (output, 0),
                    ContentBlockStartData::ToolCall {
                        id: item["id"].as_str().unwrap_or("").into(),
                        call_id: item["call_id"].as_str().map(str::to_string),
                        name: item["name"].as_str().unwrap_or("").into(),
                    },
                    &mut events,
                );
            }
            "response.output_text.delta" => {
                let index = self.start((output, content), ContentBlockStartData::Text, &mut events);
                let text = event["delta"].as_str().unwrap_or("").into();
                events.push(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::Text(text),
                });
            }
            "response.refusal.delta" => {
                self.refusal
                    .get_or_insert_with(String::new)
                    .push_str(event["delta"].as_str().unwrap_or(""));
            }
            "response.refusal.done" => {
                let accumulated = self.refusal.take().unwrap_or_default();
                let refusal = event["refusal"]
                    .as_str()
                    .map_or(accumulated, str::to_string);
                return Err(GenerationError::Refusal(refusal));
            }
            "response.function_call_arguments.delta" => {
                let index = *self.blocks.get(&(output, 0)).ok_or_else(|| {
                    GenerationError::InvalidResponse(
                        "tool arguments arrived without a tool call".into(),
                    )
                })?;
                events.push(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::ToolCall {
                        arguments: event["delta"].as_str().unwrap_or("").into(),
                    },
                });
            }
            "response.reasoning_summary_text.delta" => {
                let index = self.start((output, 0), ContentBlockStartData::Reasoning, &mut events);
                let reasoning = event["delta"].as_str().unwrap_or("").into();
                events.push(StreamEvent::ContentBlockDelta {
                    index,
                    delta: ContentBlockDelta::Reasoning(reasoning),
                });
            }
            "response.output_item.done" => {
                if event["item"]["type"] == "reasoning"
                    && let Some(signature) = event["item"]["encrypted_content"].as_str()
                {
                    let index =
                        self.start((output, 0), ContentBlockStartData::Reasoning, &mut events);
                    events.push(StreamEvent::ContentBlockDelta {
                        index,
                        delta: ContentBlockDelta::Signature(format!("{}{signature}", self.scope)),
                    });
                }
                let keys: Vec<_> = self
                    .blocks
                    .keys()
                    .filter(|(item_output, _)| *item_output == output)
                    .copied()
                    .collect();
                for key in keys {
                    if let Some(index) = self.blocks.remove(&key) {
                        events.push(StreamEvent::ContentBlockStop { index });
                    }
                }
            }
            "response.completed" => {
                let stop_reason = if self.tools {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                };
                self.finish(event, stop_reason, false, &mut events)?;
            }
            "response.incomplete" => {
                let stop_reason = match event["response"]["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => StopReason::MaxOutputTokens,
                    Some("content_filter") => StopReason::ContentFilter,
                    reason => {
                        return Err(GenerationError::InvalidResponse(format!(
                            "the model response is incomplete ({}); retry or choose another model",
                            reason.unwrap_or("no reason given")
                        )));
                    }
                };
                self.finish(event, stop_reason, true, &mut events)?;
            }
            "response.failed" | "error" => {
                let code = event["response"]["error"]["code"]
                    .as_str()
                    .or_else(|| event["error"]["code"].as_str())
                    .or_else(|| event["code"].as_str())
                    .unwrap_or("");
                return Err(service_error(code));
            }
            _ => {}
        }
        Ok(events)
    }

    /// End the message. A cut-off response closes its open blocks; a
    /// completed one must have closed them already.
    fn finish(
        &mut self,
        event: &Value,
        stop_reason: StopReason,
        cut_off: bool,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), GenerationError> {
        if cut_off {
            for (_, index) in std::mem::take(&mut self.blocks) {
                events.push(StreamEvent::ContentBlockStop { index });
            }
        }
        if !self.blocks.is_empty() {
            return Err(GenerationError::InvalidResponse(
                "the response completed with unfinished content blocks".into(),
            ));
        }
        let response = &event["response"];
        let usage = &response["usage"];
        let cached = usage["input_tokens_details"]["cached_tokens"].as_u64();
        events.push(StreamEvent::MessageStop {
            stop_reason,
            usage: Usage {
                input_tokens: usage["input_tokens"]
                    .as_u64()
                    .map(|tokens| tokens.saturating_sub(cached.unwrap_or(0))),
                output_tokens: usage["output_tokens"].as_u64(),
                total_tokens: usage["total_tokens"].as_u64(),
                cache_read_tokens: cached,
                cache_creation_tokens: None,
                reasoning_output_tokens: usage["output_tokens_details"]["reasoning_tokens"]
                    .as_u64(),
            },
            id: response["id"].as_str().map(str::to_string),
            model: response["model"].as_str().map(str::to_string),
        });
        self.completed = true;
        Ok(())
    }
}

/// The user-facing message for a `ChatGPT` plan error code.
pub(crate) fn error_message(code: &str) -> String {
    match code {
        PLAN_LIMIT_CODE => format!(
            "ChatGPT usage limit reached; manage usage at {} or choose another connection",
            super::USAGE_URL
        ),
        "subscription_sharing_user_not_eligible" => "ChatGPT plan usage is unavailable for this account, workspace, or policy; choose another connection".into(),
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => "ChatGPT usage is temporarily unavailable; retry later (saved credentials were kept)".into(),
        "subscription_sharing_invalid_user" => "ChatGPT authentication could not be validated; sign in to ChatGPT again".into(),
        "subscription_sharing_unsupported_capability" => "the selected ChatGPT model or request capability is unavailable; choose another model or connection".into(),
        _ => "the model service rejected the request; check your account and model settings".into(),
    }
}

/// Error code for an exhausted `ChatGPT` plan or app usage limit.
pub(crate) const PLAN_LIMIT_CODE: &str = "subscription_sharing_usage_limit_exceeded";

/// Source of the [`GenerationError::Provider`] for an exhausted plan limit.
///
/// Recognise it with [`is_plan_limit`] rather than by message text.
#[derive(Debug)]
pub(crate) struct PlanUsageLimit;

impl std::fmt::Display for PlanUsageLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(PLAN_LIMIT_CODE)
    }
}

impl std::error::Error for PlanUsageLimit {}

/// Whether `error` reports an exhausted `ChatGPT` plan limit.
pub(crate) fn is_plan_limit(error: &GenerationError) -> bool {
    matches!(error, GenerationError::Provider { source: Some(source), .. } if source.is::<PlanUsageLimit>())
}

fn service_error(code: &str) -> GenerationError {
    match code {
        // Plan limits need user action, even when reported as HTTP 429, so
        // carry no status that would invite an automatic retry.
        PLAN_LIMIT_CODE => GenerationError::Provider {
            status: None,
            message: error_message(code),
            source: Some(Box::new(PlanUsageLimit)),
        },
        "subscription_sharing_usage_unavailable" | "subscription_sharing_user_unavailable" => {
            GenerationError::Provider {
                status: Some(503),
                message: error_message(code),
                source: None,
            }
        }
        _ => GenerationError::InvalidRequest(error_message(code)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris::models::llm::{ReasoningBlock, StreamEventExt, ToolDefinition};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn decode(decoder: &mut Decoder, events: &[Value]) -> Vec<StreamEvent> {
        events
            .iter()
            .flat_map(|event| decoder.event(event).expect("event decodes"))
            .collect()
    }

    async fn collect(events: Vec<StreamEvent>) -> polaris::models::llm::LlmResponse {
        futures_util::stream::iter(events.into_iter().map(Ok))
            .collect_response()
            .await
            .expect("events form a response")
    }

    fn reasoning_request(signature: &str) -> LlmRequest {
        LlmRequest {
            messages: vec![Message::Assistant {
                id: None,
                content: vec![AssistantBlock::Reasoning(ReasoningBlock {
                    id: None,
                    reasoning: vec![],
                    signature: Some(signature.into()),
                })],
            }],
            ..Default::default()
        }
    }

    /// Serve one HTTP response with `head` and `body` and return what a client receives.
    async fn reply(head: &str, body: &str) -> reqwest::Response {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        let response = format!(
            "HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request = [0; 4096];
            let _ = socket.read(&mut request).await.expect("read request");
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        reqwest::get(format!("http://{address}"))
            .await
            .expect("reach test server")
    }

    async fn stream_error(head: &str, body: &str) -> GenerationError {
        response_stream(reply(head, body).await, "scope:".into())
            .await
            .err()
            .expect("a failed status is an error")
    }

    #[test]
    fn request_body_is_stateless_streaming_and_namespaced() {
        use polaris::models::llm::ReasoningEffort;
        let body = request_body(
            "model",
            &LlmRequest {
                system: Some("instructions".into()),
                messages: vec![Message::user("hello")],
                tools: Some(vec![ToolDefinition::new(
                    "read_file",
                    "Read",
                    json!({"type":"object"}),
                )]),
                reasoning_effort: Some(ReasoningEffort::High),
                max_output_tokens: std::num::NonZeroU32::new(20),
                ..Default::default()
            },
            "test-scope:",
        )
        .expect("request encodes");
        assert_eq!(body["store"], false, "requests are not stored");
        assert_eq!(body["stream"], true, "requests stream");
        assert_eq!(
            body["instructions"], "instructions",
            "the system prompt is sent"
        );
        assert_eq!(
            body["tools"][0]["type"], "namespace",
            "tools are namespaced"
        );
        assert_eq!(
            body["tools"][0]["name"], TOOL_NAMESPACE,
            "the namespace is named"
        );
        assert_eq!(
            body["reasoning"]["effort"], "high",
            "reasoning effort is kept"
        );
        assert!(
            body.get("max_output_tokens").is_none(),
            "the plan backend applies its own output limit"
        );
        assert!(
            body.get("previous_response_id").is_none(),
            "no server-side conversation state is used"
        );
    }

    #[test]
    fn request_body_requires_call_ids() {
        let mut call = polaris::models::llm::ToolCall::new("item-1", "read_file", json!({}));
        let request = |call: &polaris::models::llm::ToolCall| LlmRequest {
            messages: vec![Message::Assistant {
                id: None,
                content: vec![AssistantBlock::ToolCall(call.clone())],
            }],
            ..Default::default()
        };
        assert!(
            matches!(
                request_body("model", &request(&call), "scope:"),
                Err(GenerationError::InvalidRequest(_))
            ),
            "an item id is not used in place of a call id"
        );
        call.call_id = Some("call-1".into());
        let body = request_body("model", &request(&call), "scope:").expect("call encodes");
        assert_eq!(body["input"][0]["call_id"], "call-1", "the call id is sent");
        assert_eq!(
            body["input"][0]["namespace"], TOOL_NAMESPACE,
            "the call names its namespace"
        );
        call.call_id = None;
        let result = LlmRequest {
            messages: vec![Message::User {
                content: vec![call.result(ToolResultContent::Text("done".into()))],
            }],
            ..Default::default()
        };
        assert!(
            matches!(
                request_body("model", &result, "scope:"),
                Err(GenerationError::InvalidRequest(_))
            ),
            "a tool result without a call id is refused"
        );
    }

    #[test]
    fn request_body_sends_document_mime_types() {
        let request = LlmRequest {
            messages: vec![Message::User {
                content: vec![UserBlock::document_base64(
                    "report.pdf",
                    "QQ==",
                    DocumentMediaType::PDF,
                )],
            }],
            ..Default::default()
        };
        let body = request_body("model", &request, "scope:").expect("document encodes");
        assert_eq!(
            body["input"][0]["content"][0]["file_data"], "data:application/pdf;base64,QQ==",
            "the document's own MIME type is used"
        );
    }

    #[test]
    fn request_body_replays_only_this_accounts_reasoning() {
        let account_a = reasoning_scope("account-a");
        let account_b = reasoning_scope("account-b");
        let mut decoder = Decoder {
            scope: account_a.clone(),
            ..Default::default()
        };
        let signature = decode(
            &mut decoder,
            &[
                json!({"type":"response.output_item.done", "item":{"type":"reasoning","encrypted_content":"ciphertext"}}),
            ],
        )
        .into_iter()
        .find_map(|event| match event {
            StreamEvent::ContentBlockDelta {
                delta: ContentBlockDelta::Signature(signature),
                ..
            } => Some(signature),
            _ => None,
        })
        .expect("encrypted reasoning becomes a signature");
        assert!(
            is_chatgpt_reasoning(&signature),
            "the signature is recognisable as ChatGPT reasoning"
        );
        let request = reasoning_request(&signature);
        assert_eq!(
            request_body("model", &request, &account_a).expect("request encodes")["input"][0]["encrypted_content"],
            "ciphertext",
            "the issuing account replays its reasoning without the tag"
        );
        for scope in [account_b.as_str(), "foreign-signature"] {
            let body = request_body("model", &request, scope).expect("request encodes");
            assert_eq!(
                body["input"].as_array().map(Vec::len),
                Some(0),
                "reasoning is not sent under scope {scope}"
            );
        }
        assert!(
            !is_chatgpt_reasoning("anthropic-signature"),
            "other providers' signatures are not ChatGPT reasoning"
        );
    }

    #[test]
    fn stream_bounds_reject_oversized_events_across_chunks() {
        let mut bounds = StreamBounds::default();
        bounds
            .check(&vec![b'x'; MAX_EVENT_BYTES - 1])
            .expect("an event under the limit is admitted");
        assert!(
            matches!(
                bounds.check(b"xx"),
                Err(GenerationError::InvalidResponse(_))
            ),
            "the limit applies across chunks"
        );
        let mut bounds = StreamBounds::default();
        bounds
            .check(b"data: first\r\n\r")
            .expect("first chunk is admitted");
        bounds
            .check(b"\ndata: second\n\n")
            .expect("second chunk is admitted");
        assert_eq!(bounds.event_bytes, 0, "a blank line resets the event size");
    }

    #[tokio::test]
    async fn decoder_completes_text_and_tool_streams() {
        let mut decoder = Decoder::default();
        let events = decode(
            &mut decoder,
            &[
                json!({"type":"response.output_text.delta","output_index":0,"delta":"Hello"}),
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message"}}),
                json!({"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"item-1","call_id":"call-1","name":"read_file"}}),
                json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":\"README.md\"}"}),
                json!({"type":"response.output_item.done","output_index":1,"item":{"type":"function_call"}}),
                json!({"type":"response.completed","response":{"id":"resp-1","model":"model-1","usage":{"input_tokens":100,"output_tokens":12,"total_tokens":112,"input_tokens_details":{"cached_tokens":40}}}}),
            ],
        );
        let response = collect(events).await;
        assert_eq!(
            response.stop_reason,
            StopReason::ToolUse,
            "a tool call ends the turn"
        );
        assert_eq!(
            response.usage.input_tokens,
            Some(60),
            "cached input is excluded"
        );
        assert_eq!(
            response.usage.cache_read_tokens,
            Some(40),
            "cached input is reported"
        );
        let AssistantBlock::ToolCall(call) = &response.content[1] else {
            panic!("the tool call is retained after the text");
        };
        assert_eq!(
            call.call_id.as_deref(),
            Some("call-1"),
            "the call id is kept"
        );
        assert_eq!(
            call.function.arguments["path"], "README.md",
            "arguments are assembled"
        );
    }

    #[tokio::test]
    async fn decoder_ends_cut_off_responses_with_their_reason() {
        for (reason, stop_reason) in [
            ("max_output_tokens", StopReason::MaxOutputTokens),
            ("content_filter", StopReason::ContentFilter),
        ] {
            let mut decoder = Decoder::default();
            let events = decode(
                &mut decoder,
                &[
                    json!({"type":"response.output_text.delta", "delta":"Partial answer"}),
                    json!({"type":"response.incomplete", "response":{"incomplete_details":{"reason":reason}, "usage":{"input_tokens":4,"output_tokens":5}}}),
                ],
            );
            assert!(decoder.completed, "{reason}: the message ends");
            let response = collect(events).await;
            assert_eq!(response.stop_reason, stop_reason, "{reason}: stop reason");
            assert_eq!(
                response.usage.output_tokens,
                Some(5),
                "{reason}: usage is kept"
            );
            assert!(
                matches!(&response.content[0], AssistantBlock::Text(text) if text.text == "Partial answer"),
                "{reason}: partial text is kept for continuation"
            );
        }
    }

    #[test]
    fn decoder_rejects_incomplete_responses_for_other_reasons() {
        for details in [json!({"reason":"server_shutdown"}), json!(null)] {
            let mut decoder = Decoder::default();
            let error = decoder
                .event(&json!({"type":"response.incomplete", "response":{"incomplete_details":details}}))
                .expect_err("an unhandled cut-off is an error");
            assert!(
                matches!(error, GenerationError::InvalidResponse(_)),
                "{details}: reported as an invalid response"
            );
            assert!(
                !decoder.completed,
                "{details}: not a successful message stop"
            );
        }
    }

    #[test]
    fn decoder_reports_refusals() {
        let mut decoder = Decoder::default();
        decoder
            .event(&json!({"type":"response.refusal.delta", "delta":"I can't "}))
            .expect("refusal delta accumulates");
        decoder
            .event(&json!({"type":"response.refusal.delta", "delta":"help."}))
            .expect("refusal delta accumulates");
        let error = decoder
            .event(&json!({"type":"response.refusal.done"}))
            .expect_err("a finished refusal is an error");
        assert!(
            matches!(&error, GenerationError::Refusal(text) if text == "I can't help."),
            "the accumulated refusal text is reported: {error}"
        );
    }

    #[test]
    fn decoder_reports_plan_limit_by_type() {
        let mut decoder = Decoder::default();
        decoder
            .event(&json!({"type":"response.output_text.delta","delta":"partial"}))
            .expect("text decodes");
        let error = decoder
            .event(&json!({"type":"response.failed","response":{"error":{"code":PLAN_LIMIT_CODE}}}))
            .expect_err("a late failure is an error");
        assert!(!decoder.completed, "a failure never becomes a message stop");
        assert!(
            is_plan_limit(&error),
            "the plan limit is recognisable by type"
        );
        let failure = crate::agents::failure::describe(
            &error,
            "chatgpt/model",
            crate::agents::failure::Attempts::default(),
        );
        assert!(!failure.kind.is_transient(), "a plan limit is not retried");
        assert!(
            failure.message.contains(PLAN_LIMIT_CODE) && failure.message.contains("manage usage"),
            "the summary names the code and the fix: {}",
            failure.message
        );
    }

    #[tokio::test]
    async fn response_stream_maps_failed_statuses() {
        let error = stream_error(
            "401 Unauthorized",
            r#"{"error":{"message":"token revoked for sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm"}}"#,
        )
        .await;
        let GenerationError::Auth(message) = &error else {
            panic!("401 is an authentication error: {error}");
        };
        assert!(
            message.contains("token revoked"),
            "the server's reason is kept"
        );
        assert!(
            !message.contains("sk-ant-api03"),
            "secrets in the server's reason are redacted"
        );
        assert!(
            matches!(
                stream_error("429 Too Many Requests\r\nRetry-After: 7", "{}").await,
                GenerationError::RateLimited { retry_after: Some(wait) } if wait.as_secs() == 7
            ),
            "429 is a rate limit with the requested wait"
        );
        assert!(
            matches!(
                stream_error("503 Service Unavailable", "{}").await,
                GenerationError::Provider {
                    status: Some(503),
                    ..
                }
            ),
            "5xx is a retryable provider error"
        );
        let error = stream_error(
            "400 Bad Request",
            r#"{"error":{"message":"unknown model"}}"#,
        )
        .await;
        assert!(
            matches!(&error, GenerationError::InvalidRequest(message) if message.contains("unknown model")),
            "other 4xx keep the server's reason: {error}"
        );
        let body = format!(r#"{{"error":{{"code":"{PLAN_LIMIT_CODE}"}}}}"#);
        assert!(
            is_plan_limit(&stream_error("429 Too Many Requests", &body).await),
            "a plan code outranks the 429 status"
        );
        assert!(
            matches!(
                stream_error(
                    "403 Forbidden",
                    r#"{"error":{"code":"subscription_sharing_user_unavailable"}}"#
                )
                .await,
                GenerationError::Provider {
                    status: Some(503),
                    ..
                }
            ),
            "temporarily unavailable plan usage is retryable"
        );
    }

    #[tokio::test]
    async fn response_stream_fails_without_completion() {
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n";
        let stream = response_stream(
            reply("200 OK\r\nContent-Type: text/event-stream", body).await,
            "scope:".into(),
        )
        .await
        .expect("a success status streams");
        let events: Vec<_> = stream.collect().await;
        assert!(
            matches!(
                events.last(),
                Some(Err(GenerationError::InvalidResponse(_)))
            ),
            "a stream without response.completed ends in an error"
        );
        assert!(
            events.iter().rev().skip(1).all(Result::is_ok),
            "the partial text precedes the error"
        );
    }
}
