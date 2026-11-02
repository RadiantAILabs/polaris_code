//! HTTP backend with byte limits applied before JSON or SSE parsing.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{StreamExt, stream::BoxStream};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderName, HeaderValue};
use rmcp::model::{
    ClientJsonRpcMessage, ClientRequest, ErrorData, JsonRpcMessage, ServerJsonRpcMessage,
};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use sse_stream::{Error as SseError, Sse, SseStream};

// Separate from the displayed 32 KiB result budget: catalogs and structured
// results need room for protocol envelopes, but never unbounded buffering.
pub(super) const MAX_HTTP_BYTES: usize = 1024 * 1024;
type Error = StreamableHttpError<reqwest::Error>;

#[derive(Clone)]
pub(super) struct BoundedHttpClient(reqwest::Client);

impl BoundedHttpClient {
    pub(super) fn new() -> Result<Self, reqwest::Error> {
        Ok(Self(
            reqwest::Client::builder()
                .pool_max_idle_per_host(0)
                // Preserve the SDK's same-origin, method-preserving policy.
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    let previous = attempt.previous();
                    let same_origin = previous
                        .first()
                        .is_some_and(|first| first.origin() == attempt.url().origin());
                    if previous.len() <= 10
                        && same_origin
                        && matches!(attempt.status().as_u16(), 307 | 308)
                    {
                        attempt.follow()
                    } else {
                        attempt.stop()
                    }
                }))
                .build()?,
        ))
    }
}

fn too_large() -> Error {
    Error::UnexpectedServerResponse("HTTP response exceeded 1048576 bytes".into())
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, Error> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_HTTP_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(Error::Client)? {
        if chunk.len() > MAX_HTTP_BYTES - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl StreamableHttpClient for BoundedHttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let mut request = self
            .0
            .post(uri.as_ref())
            .header(ACCEPT, "application/json, text/event-stream");
        for (name, value) in custom_headers {
            if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
                return Err(Error::ReservedHeaderConflict(name.to_string()));
            }
            request = request.header(name, value);
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header("mcp-session-id", session_id.as_ref());
        }
        if let Some(token) = auth_header {
            request = request.bearer_auth(token);
        }
        let response = request.json(&message).send().await.map_err(Error::Client)?;
        let status = response.status();
        if matches!(status.as_u16(), 202 | 204) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status.as_u16() == 404 && session_was_attached {
            return Err(Error::SessionExpired);
        }
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let is_json = content_type.starts_with("application/json");
        if response
            .content_length()
            .is_some_and(|size| size > MAX_HTTP_BYTES as u64)
        {
            return Err(too_large());
        }
        if status.is_success() && content_type.starts_with("text/event-stream") {
            // A POST stream belongs to one request. Bound all its raw bytes,
            // including comments, before the SSE parser retains any of them.
            let stream = response
                .bytes_stream()
                .scan(Some(0usize), |received, chunk| {
                    let result = received.as_mut().map(|count| {
                        let chunk = chunk.map_err(std::io::Error::other)?;
                        if chunk.len() > MAX_HTTP_BYTES - *count {
                            return Err(std::io::Error::other(
                                "MCP HTTP response exceeded byte limit",
                            ));
                        }
                        *count += chunk.len();
                        Ok(chunk)
                    });
                    if result.as_ref().is_some_and(Result::is_err) {
                        *received = None;
                    }
                    std::future::ready(result)
                });
            return Ok(StreamableHttpPostResponse::Sse(
                SseStream::from_bytes_stream(stream).boxed(),
                session_id,
            ));
        }
        // Error responses are bounded by the same reader, even when their
        // content type is wrong or their transfer encoding is chunked.
        let body = bounded_body(response).await?;
        let parsed = if is_json {
            serde_json::from_slice::<ServerJsonRpcMessage>(&body).ok()
        } else {
            None
        };
        if !status.is_success() {
            // Preserve legacy server negotiation: some middleware cannot echo
            // the discovery id. Re-correlate only sessionless 4xx discovery.
            if !session_was_attached
                && status.is_client_error()
                && !matches!(status.as_u16(), 401 | 403)
                && let ClientJsonRpcMessage::Request(request) = &message
                && matches!(request.request, ClientRequest::DiscoverRequest(_))
            {
                let error = match parsed {
                    Some(JsonRpcMessage::Error(error)) => error.error,
                    _ => ErrorData::invalid_request("server/discover rejected", None),
                };
                return Ok(StreamableHttpPostResponse::Json(
                    ServerJsonRpcMessage::error(error, Some(request.id.clone())),
                    None,
                ));
            }
            if !matches!(status.as_u16(), 401 | 403)
                && let Some(parsed @ JsonRpcMessage::Error(_)) = parsed
            {
                return Ok(StreamableHttpPostResponse::Json(parsed, session_id));
            }
            return Err(
                rmcp::transport::streamable_http_client::HttpStatusError::new(
                    status.as_u16(),
                    None,
                )
                .into(),
            );
        }
        if let Some(parsed) = parsed {
            return Ok(StreamableHttpPostResponse::Json(parsed, session_id));
        }
        if !matches!(message, ClientJsonRpcMessage::Request(_)) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        // Never put raw server bodies in errors or tracing output.
        Err(Error::UnexpectedServerResponse(
            "invalid JSON-RPC response".into(),
        ))
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxStream<'static, Result<Sse, SseError>>, Error> {
        // The SDK applies this limit to raw bytes before its SSE parser. GET
        // status failures return without reading their bodies.
        self.0
            .get_stream_with_max_sse_event_size(
                uri,
                session_id,
                last_event_id,
                auth_header,
                custom_headers,
                MAX_HTTP_BYTES,
            )
            .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        // The SDK inspects only the status and never buffers this body.
        self.0
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
    }
}
