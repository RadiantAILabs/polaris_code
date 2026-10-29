//! OTLP/HTTP trace source — accepts OpenTelemetry traces and logs over HTTP.
//!
//! [`OtlpIOPlugin`] runs an [`axum`] server on `127.0.0.1:4318` (configurable)
//! and accepts:
//!
//! - `POST /v1/traces` — OTLP/HTTP traces, JSON body
//! - `POST /v1/logs`   — OTLP/HTTP logs, JSON body
//!
//! Each `Span` or `LogRecord` in the request payload becomes one
//! [`IOMessage`] pushed to [`TraceIngestAPI`](crate::TraceIngestAPI), with
//! [`IOSource::External("otlp")`](polaris::plugins::IOSource::External) and
//! [`IOContent::Structured`](polaris::plugins::IOContent::Structured)
//! carrying the per-record JSON (including merged resource attributes).
//!
//! # Scope
//!
//! JSON encoding only — protobuf bodies return 415. Add an
//! `application/x-protobuf` arm here when needed. gRPC (port 4317) is not
//! implemented.
//!
//! # OTLP reference
//!
//! - <https://opentelemetry.io/docs/specs/otlp/#otlphttp>

use crate::plugins::trace_ingest::TraceIngestAPI;
use axum::{Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::post};
use polaris::plugins::{IOContent, IOMessage, IOSource};
use polaris::system::plugin::{Contract, Plugin, PluginAccess, Version, VersionReq};
use polaris::system::server::Server;
use serde_json::{Value, json};
use std::net::SocketAddr;
use tokio::sync::mpsc;

/// Default bind address: localhost on the OTLP/HTTP port.
pub const DEFAULT_BIND: &str = "127.0.0.1:4318";

/// `IOSource::External` discriminator stamped on every OTLP-derived message.
pub const SOURCE_TAG: &str = "otlp";

/// Source plugin that runs an embedded OTLP/HTTP JSON receiver and forwards
/// each span/log record into the ingest pipeline.
///
/// On [`ready()`](OtlpIOPlugin::ready) it binds an [`axum`] server to
/// `127.0.0.1:4318` (the OTLP/HTTP port) and decomposes incoming
/// `ResourceSpans` / `ResourceLogs` payloads into one
/// [`IOMessage`](polaris::plugins::IOMessage) per `Span` / `LogRecord`,
/// pushing each into [`TraceIngestAPI`](crate::TraceIngestAPI). Reach for it
/// when an OpenTelemetry exporter should feed traces into `ontrace`: point the
/// exporter's OTLP/HTTP endpoint at this receiver and every span and log
/// record becomes a trace line for the extraction backend.
///
/// Records are decomposed structurally — each emitted message carries the
/// per-record JSON with the enclosing `resource` and `scope` objects merged
/// in, stamped [`IOSource::External("otlp")`](polaris::plugins::IOSource::External)
/// and (when present) the record's `traceId` as metadata. JSON bodies only;
/// protobuf and gRPC (port 4317) are out of scope.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | _none_ | — | This is a transport-only source; it owns no `Server` resource. The decomposed records flow into the shared ingest channel rather than a resource this plugin publishes. |
///
/// # APIs Provided
///
/// | API | Description |
/// |-----|-------------|
/// | _none_ | This plugin provides no API. It is a pure consumer of [`TraceIngestAPI`](crate::TraceIngestAPI), into which it pushes the OTLP-derived [`IOMessage`](polaris::plugins::IOMessage)s. |
///
/// # Dependencies
///
/// Declares a capability dependency on
/// [`TraceIngestAPI`](crate::TraceIngestAPI) (provided by
/// [`TraceIngestPlugin`](crate::TraceIngestPlugin)) via `access()`. The
/// resolver validates the provider exists at `finish()`, so this source is
/// indifferent to which extraction backend is wired up.
///
/// # Routes Provided
///
/// These routes are served by an embedded [`axum`] server bound to
/// `127.0.0.1:4318` (overridable via [`with_bind`](OtlpIOPlugin::with_bind));
/// they are **not** mounted on a shared `HttpRouter`.
///
/// | Method | Path | Description |
/// |--------|------|-------------|
/// | `POST` | `/v1/traces` | OTLP/HTTP traces, JSON body. Each `Span` becomes one [`IOMessage`](polaris::plugins::IOMessage). |
/// | `POST` | `/v1/logs` | OTLP/HTTP logs, JSON body. Each `LogRecord` becomes one [`IOMessage`](polaris::plugins::IOMessage). |
///
/// # Lifecycle
///
/// [`ready()`](OtlpIOPlugin::ready) fetches the
/// [`TraceIngestAPI`](crate::TraceIngestAPI) sender, builds the router, and
/// spawns a [`tokio`] task that binds a `TcpListener` and serves the OTLP
/// endpoints. The bind defaults to [`DEFAULT_BIND`] (`127.0.0.1:4318`) and is
/// configurable only at construction via [`with_bind`](OtlpIOPlugin::with_bind);
/// [`new`](OtlpIOPlugin::new) / [`Default`] use the default address. A bind
/// failure is logged and aborts the server task without panicking.
///
/// # Example
///
/// ```no_run
/// use std::sync::Arc;
/// use polaris::models::{AnthropicPlugin, ModelsPlugin};
/// use polaris::plugins::PersistencePlugin;
/// use polaris::sessions::SessionsPlugin;
/// use polaris::sessions::store::memory::InMemoryStore;
/// use polaris::system::server::Server;
/// use polaris_ontrace::{ExtractionPlugin, OtlpIOPlugin, TraceIngestPlugin};
///
/// let mut server = Server::new();
/// server
///     .add_plugins(PersistencePlugin)
///     .add_plugins(ModelsPlugin)
///     .add_plugins(AnthropicPlugin::from_env("ANTHROPIC_API_KEY"))
///     .add_plugins(SessionsPlugin::new(Arc::new(InMemoryStore::new())))
///     .add_plugins(TraceIngestPlugin::new())   // provides TraceIngestAPI
///     .add_plugins(ExtractionPlugin::new())    // a backend claims the receiver
///     .add_plugins(OtlpIOPlugin::new());        // listens on 127.0.0.1:4318
/// ```
pub struct OtlpIOPlugin {
    bind: SocketAddr,
}

impl OtlpIOPlugin {
    /// Construct with the default bind address ([`DEFAULT_BIND`]).
    ///
    /// # Panics
    ///
    /// Panics if [`DEFAULT_BIND`] fails to parse — it is a compile-time
    /// constant so this is unreachable in practice.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bind: DEFAULT_BIND.parse().expect("DEFAULT_BIND parses"),
        }
    }

    /// Construct bound to a specific address.
    #[must_use]
    pub fn with_bind(bind: SocketAddr) -> Self {
        Self { bind }
    }
}

impl Default for OtlpIOPlugin {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for OtlpIOPlugin {
    const ID: &'static str = "ontrace::sources::otlp";
    const VERSION: Version = Version::new(0, 1, 0);

    fn build(&self, _server: &mut Server) {}

    async fn ready(&self, server: &mut Server) {
        let tx = server
            .api::<TraceIngestAPI>()
            .expect("OtlpIOPlugin requires TraceIngestPlugin")
            .sender();
        let bind = self.bind;

        let app = Router::new()
            .route("/v1/traces", post(handle_traces))
            .route("/v1/logs", post(handle_logs))
            .with_state(tx);

        tokio::spawn(async move {
            let listener = match tokio::net::TcpListener::bind(bind).await {
                Ok(l) => l,
                Err(err) => {
                    tracing::error!(error = %err, %bind, "OTLP: bind failed");
                    return;
                }
            };
            tracing::info!(%bind, "OTLP/HTTP receiver listening");
            if let Err(err) = axum::serve(listener, app).await {
                tracing::error!(error = %err, "OTLP: server stopped");
            }
        });
    }

    fn access(&self) -> PluginAccess {
        PluginAccess::new()
            .requires::<TraceIngestAPI>(VersionReq::caret(TraceIngestAPI::CONTRACT_VERSION))
    }
}

async fn handle_traces(
    State(tx): State<mpsc::Sender<IOMessage>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let count = decompose_traces(&payload, &tx).await;
    tracing::debug!(spans = count, "OTLP traces received");
    (StatusCode::OK, Json(json!({})))
}

async fn handle_logs(
    State(tx): State<mpsc::Sender<IOMessage>>,
    Json(payload): Json<Value>,
) -> impl IntoResponse {
    let count = decompose_logs(&payload, &tx).await;
    tracing::debug!(records = count, "OTLP logs received");
    (StatusCode::OK, Json(json!({})))
}

/// Walks an OTLP traces payload, emitting one [`IOMessage`] per span with
/// resource + scope context merged in. Returns the number of spans emitted.
async fn decompose_traces(payload: &Value, tx: &mpsc::Sender<IOMessage>) -> usize {
    let mut count = 0;
    let Some(resource_spans) = payload.get("resourceSpans").and_then(|v| v.as_array()) else {
        return 0;
    };
    for rs in resource_spans {
        let resource = rs.get("resource").cloned().unwrap_or(Value::Null);
        let Some(scope_spans) = rs.get("scopeSpans").and_then(|v| v.as_array()) else {
            continue;
        };
        for ss in scope_spans {
            let scope = ss.get("scope").cloned().unwrap_or(Value::Null);
            let Some(spans) = ss.get("spans").and_then(|v| v.as_array()) else {
                continue;
            };
            for span in spans {
                let record = json!({
                    "kind": "span",
                    "resource": resource,
                    "scope": scope,
                    "span": span,
                });
                let message = build_message(&record, span.get("traceId"));
                if tx.send(message).await.is_err() {
                    tracing::info!("trace channel closed; OTLP handler aborting");
                    return count;
                }
                count += 1;
            }
        }
    }
    count
}

/// Walks an OTLP logs payload, emitting one [`IOMessage`] per record.
async fn decompose_logs(payload: &Value, tx: &mpsc::Sender<IOMessage>) -> usize {
    let mut count = 0;
    let Some(resource_logs) = payload.get("resourceLogs").and_then(|v| v.as_array()) else {
        return 0;
    };
    for rl in resource_logs {
        let resource = rl.get("resource").cloned().unwrap_or(Value::Null);
        let Some(scope_logs) = rl.get("scopeLogs").and_then(|v| v.as_array()) else {
            continue;
        };
        for sl in scope_logs {
            let scope = sl.get("scope").cloned().unwrap_or(Value::Null);
            let Some(records) = sl.get("logRecords").and_then(|v| v.as_array()) else {
                continue;
            };
            for record in records {
                let payload = json!({
                    "kind": "log",
                    "resource": resource,
                    "scope": scope,
                    "log": record,
                });
                let message = build_message(&payload, record.get("traceId"));
                if tx.send(message).await.is_err() {
                    tracing::info!("trace channel closed; OTLP handler aborting");
                    return count;
                }
                count += 1;
            }
        }
    }
    count
}

fn build_message(payload: &Value, trace_id: Option<&Value>) -> IOMessage {
    let mut message = IOMessage::new(
        IOContent::Structured(payload.clone()),
        IOSource::External(SOURCE_TAG.into()),
    );
    if let Some(tid) = trace_id.and_then(|v| v.as_str()) {
        message = message.with_metadata(crate::plugins::trace_ingest::META_TRACE_ID, tid);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decompose_traces_emits_one_message_per_span() {
        let (tx, mut rx) = mpsc::channel(8);
        let payload = json!({
            "resourceSpans": [{
                "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "billing"}}]},
                "scopeSpans": [{
                    "scope": {"name": "scope-a"},
                    "spans": [
                        {"traceId": "aaaa", "spanId": "1111", "name": "checkout"},
                        {"traceId": "aaaa", "spanId": "2222", "name": "charge"},
                    ]
                }]
            }]
        });
        let count = decompose_traces(&payload, &tx).await;
        assert_eq!(count, 2);
        drop(tx);
        let m1 = rx.recv().await.unwrap();
        assert!(matches!(m1.source, IOSource::External(ref s) if s == SOURCE_TAG));
        assert_eq!(
            m1.metadata
                .get(crate::plugins::trace_ingest::META_TRACE_ID)
                .unwrap(),
            "aaaa"
        );
    }

    #[tokio::test]
    async fn decompose_logs_emits_one_message_per_record() {
        let (tx, mut rx) = mpsc::channel(8);
        let payload = json!({
            "resourceLogs": [{
                "scopeLogs": [{
                    "logRecords": [
                        {"body": {"stringValue": "hello"}},
                        {"body": {"stringValue": "world"}},
                    ]
                }]
            }]
        });
        assert_eq!(decompose_logs(&payload, &tx).await, 2);
        drop(tx);
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }
}
