//! Compare framework and session command checks for JSON lines on stdin.
use polaris::shell::{ShellConfig, ShellExecutor, ShellPermission};
use polaris_code::tools::command_check::checked_commands;
use std::io::{self, BufRead};
fn main() {
    for line in io::stdin().lock().lines() {
        let line = line.expect("stdin");
        let value: serde_json::Value = serde_json::from_str(&line).expect("JSON record");
        let patterns: Vec<String> =
            serde_json::from_value(value["patterns"].clone()).expect("patterns");
        let command = value["command"].as_str().expect("command");
        let executor = ShellExecutor::new(
            ShellConfig::new()
                .with_allowed_commands(patterns.clone())
                .disable_cache()
                .disable_sandbox(),
        );
        let parts = checked_commands(command, &patterns);
        println!(
            "{}",
            serde_json::json!({"old": executor.check_permission(command) == ShellPermission::Allow, "new": parts.is_some(), "parts":parts})
        );
    }
}
