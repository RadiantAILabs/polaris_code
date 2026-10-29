//! Read-only terminal pages derived from the canonical `ContextManager`.
//!
//! The driver refreshes this cache after resume and between completed turns,
//! before entering the next turn (which can hold the session context while
//! awaiting input). Browsing therefore never waits for the running graph's
//! context lock. This is an in-process projection cache, not a second store.
//! Live partial output stays in the panel's live view; history contains only
//! canonical messages captured at the last turn boundary.

use std::sync::{Arc, RwLock};

use polaris::models::llm::{AssistantBlock, Message, ToolResultContent, UserBlock};
use polaris::plugins::{IO_TYPE, IOContent, IOMessage, IOSource};
use polaris::sessions::{SessionId, SessionsAPI};
use polaris_tui::panels::conversation::{HistoryFuture, HistoryPage, TranscriptHistory};

use crate::agents::ContextManager;

/// A bounded-page view over one session's canonical conversation.
///
/// Construct one per active session, supply it to the conversation panel, and
/// call [`Self::refresh`] after resume and after each turn, before the next
/// `process_turn_with`. No request starts a graph, dispatches IO, or runs tools.
#[derive(Clone, Default)]
pub struct CanonicalHistory {
    messages: Arc<RwLock<Option<Vec<Message>>>>,
}

impl CanonicalHistory {
    /// Update the cache at a session boundary, preserving canonical order and
    /// repeated identical messages. Context access failures invalidate the cache
    /// so a subsequent read cannot silently return stale history.
    pub async fn refresh(&self, sessions: &SessionsAPI, id: &SessionId) -> Result<(), String> {
        let snapshot = sessions
            .with_context(id, |ctx| {
                ctx.get_resource::<ContextManager>()
                    .map(|context| context.messages.clone())
                    .map_err(|err| err.to_string())
            })
            .await
            .map_err(|err| err.to_string())
            .and_then(|value| value);
        match snapshot {
            Ok(messages) => {
                *self.messages.write().map_err(|err| err.to_string())? = Some(messages);
                Ok(())
            }
            Err(err) => {
                *self.messages.write().map_err(|err| err.to_string())? = None;
                Err(err)
            }
        }
    }

    /// Return the full cached canonical conversation for a visible replay.
    ///
    /// This only projects persisted model messages. It performs no IO, graph
    /// execution, or tool call.
    pub fn replay(&self) -> Result<Vec<IOMessage>, String> {
        let guard = self.messages.read().map_err(|err| err.to_string())?;
        let messages = guard
            .as_ref()
            .ok_or("Canonical history is not available yet")?;
        Ok(messages
            .iter()
            .enumerate()
            .map(|(index, message)| project(index, message))
            .collect())
    }
}

impl TranscriptHistory for CanonicalHistory {
    fn load(&self, before: Option<usize>, limit: usize) -> HistoryFuture {
        let state = self.messages.clone();
        Box::pin(async move {
            let (start, total, messages) = {
                let guard = state.read().map_err(|err| err.to_string())?;
                let messages = guard
                    .as_ref()
                    .ok_or("Canonical history is not available yet")?;
                let total = messages.len();
                let end = before.unwrap_or(total).min(total);
                let start = end.saturating_sub(limit.min(1024));
                (start, total, messages[start..end].to_vec())
            };
            let messages = messages
                .iter()
                .enumerate()
                .map(|(i, message)| project(start + i, message))
                .collect();
            Ok(HistoryPage {
                start,
                total,
                messages,
            })
        })
    }
}

/// One canonical message is one page entry. Tool result containers are not
/// labelled as user prompts; reasoning blocks are not exposed as chat text.
fn project(index: usize, message: &Message) -> IOMessage {
    let (source, kind, body) = match message {
        Message::Assistant { content, .. } => {
            let text = content
                .iter()
                .filter_map(|block| match block {
                    AssistantBlock::Text(text) => Some(text.text.clone()),
                    AssistantBlock::ToolCall(call) => Some(format!(
                        "{}({})",
                        call.function.name, call.function.arguments
                    )),
                    AssistantBlock::Reasoning(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (IOSource::Agent("code".into()), "message", text)
        }
        Message::User { content } => {
            let has_prompt = content
                .iter()
                .any(|block| !matches!(block, UserBlock::ToolResult(_)));
            let body = content
                .iter()
                .map(|block| match block {
                    UserBlock::Text(text) => text.text.clone(),
                    UserBlock::ToolResult(result) => match &result.content {
                        ToolResultContent::Text(text) => text.clone(),
                        ToolResultContent::Image(_) => "[image tool result]".into(),
                    },
                    UserBlock::Image(_) => "[image]".into(),
                    UserBlock::Audio(_) => "[audio]".into(),
                    UserBlock::Document(document) => format!("[document: {}]", document.name),
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                if has_prompt {
                    IOSource::User
                } else {
                    IOSource::System
                },
                if has_prompt { "message" } else { "tool" },
                body,
            )
        }
    };
    IOMessage::new(IOContent::Text(body), source)
        .with_metadata(IO_TYPE, kind)
        .with_metadata("id", format!("history:{index}"))
        .with_metadata("label", "recorded tool result")
        .with_metadata("phase", "recorded")
}
