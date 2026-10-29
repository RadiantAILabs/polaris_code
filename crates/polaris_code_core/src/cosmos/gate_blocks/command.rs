//! Lean's command adapter over the same host shell used by `run_python`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use polaris::shell::{
    ShellEvent, ShellEventDelivery, ShellExecutor, ShellPermission, ShellRequest, ShellResponse,
    ShellSink, ShellSinkHandle, ShellStream,
};

const TIMEOUT_SECS: u64 = 300;
const HALF_STREAM_BYTES: usize = 2_400;

/// Capture head and tail from live events, including bytes beyond the executor's
/// ordinary prefix capture limit. At most 9,600 bytes retained across both pipes.
#[derive(Default)]
struct HeadTail {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    omitted: bool,
}

impl HeadTail {
    fn append(&mut self, bytes: &[u8]) {
        let head_len = bytes.len().min(HALF_STREAM_BYTES - self.head.len());
        self.head.extend_from_slice(&bytes[..head_len]);
        for byte in &bytes[head_len..] {
            if self.tail.len() == HALF_STREAM_BYTES {
                self.tail.pop_front();
                self.omitted = true;
            }
            self.tail.push_back(*byte);
        }
    }

    fn render(&self) -> String {
        let mut bytes = self.head.clone();
        if self.omitted {
            bytes.extend_from_slice(b"\n[output clipped: head + tail]\n");
        }
        bytes.extend(self.tail.iter());
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[derive(Clone, Default)]
struct CommandCapture(Arc<Mutex<[HeadTail; 2]>>);

impl ShellSink for CommandCapture {
    fn on_event(&self, event: ShellEvent) -> ShellEventDelivery {
        if let ShellEvent::Chunk { stream, bytes, .. } = event {
            let index = match stream {
                ShellStream::Stdout => 0,
                ShellStream::Stderr => 1,
            };
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)[index]
                .append(&bytes);
        }
        ShellEventDelivery::Accepted
    }
}

/// The session executor owns permissions, process cleanup and the timeout.
/// The capture sink only renders output; it does not implement a second shell.
pub(super) async fn run_workspace_command(
    shell: Option<&ShellExecutor>,
    working_dir: &std::path::Path,
    command: &str,
) -> Result<String, String> {
    let shell = shell.ok_or("no shell is configured for this run; run_command is unavailable")?;
    if !matches!(shell.check_permission(command), ShellPermission::Allow) {
        return Err("the shell policy does not allow this command to run unattended".to_owned());
    }
    let capture = CommandCapture::default();
    let request = ShellRequest::new(command)
        .with_working_dir(working_dir.to_path_buf())
        .with_timeout(TIMEOUT_SECS)
        .with_sink(ShellSinkHandle::new(capture.clone()));
    match shell
        .execute(request)
        .await
        .map_err(|error| error.to_string())?
    {
        ShellResponse::Executed(result) => {
            let streams = capture
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let code = result
                .exit_code
                .map_or_else(|| "none".to_owned(), |n| n.to_string());
            Ok(format!(
                "exit code: {code}; timed out: {}; dropped output events: {}\nstdout:\n{}\nstderr:\n{}",
                result.timed_out,
                result.dropped_events,
                streams[0].render(),
                streams[1].render()
            ))
        }
        ShellResponse::ConfirmationRequired(_) => {
            Err("the shell policy requires confirmation; refusing unattended command".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn command_runs_in_workspace_keeps_real_tail_and_reports_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker"), "workspace-marker").unwrap();
        let shell = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec!["*".to_owned()])
                .with_max_output_bytes(128)
                .disable_cache(),
        );
        std::fs::write(
            dir.path().join("output.sh"),
            "cat marker; i=0; while [ $i -lt 12000 ]; do echo long-output-line; i=$((i+1)); done; echo final-test-summary; echo error-summary >&2; exit 7",
        ).unwrap();
        let result = run_workspace_command(Some(&shell), dir.path(), "sh output.sh")
            .await
            .unwrap();
        assert!(result.contains("workspace-marker"), "{result}");
        assert!(result.contains("final-test-summary"), "missing real tail");
        assert!(result.contains("error-summary"));
        assert!(result.contains("exit code: 7"));
        assert!(result.contains("head + tail"));
        assert!(result.chars().count() <= 10_000);
    }

    #[tokio::test]
    async fn command_refuses_missing_shell_and_unattended_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            run_workspace_command(None, dir.path(), "touch refused")
                .await
                .is_err()
        );
        let shell = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec![]),
        );
        assert!(
            run_workspace_command(Some(&shell), dir.path(), "touch refused")
                .await
                .is_err()
        );
        assert!(!dir.path().join("refused").exists());
    }
}
