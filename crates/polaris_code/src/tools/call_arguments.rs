//! The message for a tool call whose arguments cannot be used at all.
//!
//! When a model's reply stops before a tool call's JSON arguments are
//! complete — in practice, because the reply reached the output-length limit
//! while a long `old_string` or `content` was being written — the provider
//! cannot parse the arguments and the call reaches the registry with none.
//! The registry then answers `Parameters must be an object`, which says
//! nothing the model can act on. [`failure_text`] says what happened and what
//! to send instead.

use polaris::tools::ToolError;
use serde_json::Value;

/// What the model is told when the call to `tool` with `arguments` failed with
/// `error`.
///
/// The tool's own message, except for a call whose arguments were not a JSON
/// object: the registry's complaint about that says nothing the model can act
/// on, so it is replaced by an explanation of what happened.
#[must_use]
pub(crate) fn failure_text(tool: &str, arguments: &Value, error: &ToolError) -> String {
    unusable_arguments(tool, arguments).unwrap_or_else(|| error.to_string())
}

/// What to tell the model when `arguments` for `tool` are not a JSON object,
/// or `None` when they are one.
///
/// The call did not run, so the message says so, and gives the tool's own way
/// of sending less text in one call.
fn unusable_arguments(tool: &str, arguments: &Value) -> Option<String> {
    if arguments.is_object() {
        return None;
    }
    let advice = match tool {
        "edit_file" => {
            "Send smaller edits: an old_string of just the few lines that identify the \
             place, a new_string of just the lines that change, one edit per call."
        }
        "create_file" => {
            "Create the file with its first part, then add the rest with edit_file calls \
             that each carry a few functions."
        }
        "run_command" => "Send a shorter command, or split the work into several commands.",
        _ => "Send the call again with less text in it.",
    };
    Some(format!(
        "{tool} did not run: its arguments were not a complete JSON object. This usually means \
         the reply was cut off at the output-length limit while the call was being written, \
         so nothing was changed. {advice}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_object_is_left_to_the_tool() {
        assert_eq!(unusable_arguments("edit_file", &json!({})), None);
        assert_eq!(unusable_arguments("edit_file", &json!({"path": "a"})), None);
    }

    #[test]
    fn a_failure_with_usable_arguments_keeps_the_tools_own_message() {
        let error = ToolError::execution_error("old_string not found in a.py");
        assert_eq!(
            failure_text("edit_file", &json!({"path": "a.py"}), &error),
            error.to_string()
        );
        let cut_off = failure_text("edit_file", &Value::Null, &error);
        assert!(cut_off.contains("did not run"), "{cut_off}");
        assert!(!cut_off.contains("not found"), "{cut_off}");
    }

    #[test]
    fn anything_else_is_explained_with_the_tools_own_remedy() {
        for arguments in [Value::Null, json!("text"), json!([1])] {
            let message = unusable_arguments("edit_file", &arguments).unwrap();
            assert!(message.contains("did not run"), "{message}");
            assert!(message.contains("output-length limit"), "{message}");
            assert!(message.contains("smaller edits"), "{message}");
        }
        assert!(
            unusable_arguments("create_file", &Value::Null)
                .unwrap()
                .contains("first part")
        );
        assert!(
            unusable_arguments("run_command", &Value::Null)
                .unwrap()
                .contains("shorter command")
        );
        assert!(
            unusable_arguments("read_file", &Value::Null)
                .unwrap()
                .contains("less text")
        );
    }
}
