//! Ordinary graph systems contributed through the coding-agent step registry.

use super::state::{Gate, RunRules, StepState};
use polaris::models::llm::{LlmResponse, Message, ToolResultContent, UserBlock};
use polaris::plugins::{IOContent, IOMessage, IOSource, UserIO};
use polaris::system::param::{Out, Res, ResMut};
use polaris::system::system;
use polaris_code::agents::{ContextManager, FinishGate};

/// Record this plugin's actual denial results, preserving canonical text.
#[system]
pub async fn record_refusals(
    answer: Out<LlmResponse>,
    rules: Res<RunRules>,
    context: Res<ContextManager>,
) {
    let Some(Message::User { content }) = context.messages.last() else {
        return;
    };
    for block in content {
        let UserBlock::ToolResult(result) = block else {
            continue;
        };
        let ToolResultContent::Text(text) = &result.content else {
            continue;
        };
        if !text.contains("code rule \"rules.integrity\"") {
            continue;
        }
        let Some(call) = answer
            .tool_calls()
            .into_iter()
            .find(|call| call.id == result.id)
        else {
            continue;
        };
        if let Some(refusal) = rules.evaluated_refusal(text) {
            rules.refusal(
                &call.id,
                &call.function.name,
                &call.function.arguments,
                &refusal,
            );
        }
    }
}

/// Restore protected files and append findings inside the tool-result message.
#[system]
pub async fn observe_step(
    answer: Out<LlmResponse>,
    rules: Res<RunRules>,
    mut state: ResMut<StepState>,
    mut context: ResMut<ContextManager>,
    user_io: Res<UserIO>,
) {
    let calls = u32::try_from(answer.tool_calls().len()).unwrap_or(u32::MAX);
    if let Some(notice) = rules.after_step(&mut state, calls) {
        if let Some(Message::User { content }) = context.messages.last_mut() {
            content.push(UserBlock::text(notice.clone()));
        } else {
            context.push(Message::user(notice.clone()));
        }
        let _ = user_io
            .send(IOMessage::new(IOContent::Text(notice), IOSource::System))
            .await;
    }
}

/// Hold a candidate answer until the sealed check and solution scan permit it.
#[system]
pub async fn finish_gate(
    answer: Out<LlmResponse>,
    rules: Res<RunRules>,
    mut state: ResMut<StepState>,
    mut gate: ResMut<FinishGate>,
    user_io: Res<UserIO>,
) {
    let text = answer.text();
    match rules.gate(&mut state, &text).await {
        Gate::Open => {
            if let Some(report) = rules.reported_final()
                && let Some(added) = report.strip_prefix(text.as_str())
            {
                let _ = user_io
                    .send(IOMessage::new(
                        IOContent::Text(added.trim_start().to_string()),
                        IOSource::System,
                    ))
                    .await;
            }
        }
        Gate::Hold(message) => {
            let _ = user_io
                .send(IOMessage::new(
                    IOContent::Text(message.clone()),
                    IOSource::System,
                ))
                .await;
            gate.hold(message);
        }
    }
}
