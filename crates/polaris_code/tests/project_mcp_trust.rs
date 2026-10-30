//! Opening a repository must not start the programs its `.mcp.json` declares.
//!
//! These tests run the real `polaris` binary in a repository whose project
//! file declares a server that leaves a marker file when it starts. Each run
//! points the model provider at a closed loopback port, so no run reaches a
//! model; what matters is whether the marker exists once the process exits.
//! A server the user declares in their own `mcp.json` is the control: it must
//! start in the same runs, which shows the markers can be seen.

#![cfg(unix)]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long one run may take before it is stopped and the test fails.
const RUN_LIMIT: Duration = Duration::from_secs(90);

struct Setup {
    project: tempfile::TempDir,
    config: tempfile::TempDir,
    markers: tempfile::TempDir,
}

/// What a finished run left behind.
struct Run {
    stderr: String,
}

fn marker_server(marker: &Path) -> serde_json::Value {
    serde_json::json!({
        "command": "/bin/sh",
        "args": ["-c", format!("touch '{}'", marker.display())],
        "timeoutSecs": 1,
    })
}

impl Setup {
    /// A repository declaring `project_server`, and a user declaring `user_server`.
    fn new() -> Self {
        let setup = Self {
            project: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
            markers: tempfile::tempdir().unwrap(),
        };
        let declare = |dir: &Path, file: &str, name: &str| {
            let servers = serde_json::json!({ name: marker_server(&setup.marker(name)) });
            std::fs::write(
                dir.join(file),
                serde_json::json!({ "mcpServers": servers }).to_string(),
            )
            .unwrap();
        };
        declare(setup.project.path(), ".mcp.json", "project_server");
        declare(setup.config.path(), "mcp.json", "user_server");
        setup
    }

    fn marker(&self, name: &str) -> std::path::PathBuf {
        self.markers.path().join(name)
    }

    fn started(&self, name: &str) -> bool {
        self.marker(name).exists()
    }

    /// Run the binary in the repository with only the environment given here.
    fn run(&self, args: &[&str]) -> Run {
        let mut child = Command::new(env!("CARGO_BIN_EXE_polaris"))
            .args(["--provider", "openai", "--model", "openai/local-model"])
            .args(args)
            .current_dir(self.project.path())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.config.path())
            .env("POLARIS_CONFIG_DIR", self.config.path())
            .env("POLARIS_CODE_MODELS", "openai/local-model")
            .env("OPENAI_API_KEY", "not-a-real-key")
            .env("OPENAI_BASE_URL", "http://127.0.0.1:9/v1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the polaris binary starts");
        let mut pipe = child.stderr.take().unwrap();
        let reader = std::thread::spawn(move || {
            let mut text = String::new();
            pipe.read_to_string(&mut text).ok();
            text
        });
        let deadline = Instant::now() + RUN_LIMIT;
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                child.kill().ok();
                child.wait().ok();
                panic!("polaris did not exit with {args:?}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Run {
            stderr: reader.join().unwrap(),
        }
    }
}

#[test]
fn an_unattended_run_does_not_start_untrusted_project_servers() {
    let setup = Setup::new();

    let run = setup.run(&["--print", "hello"]);

    assert!(
        setup.started("user_server"),
        "the user's own server should start, so the marker can be seen:\n{}",
        run.stderr
    );
    assert!(
        !setup.started("project_server"),
        "an untrusted project server was started:\n{}",
        run.stderr
    );
    // The run says what was held back and how to decide, since no one is there
    // to be asked.
    assert!(
        run.stderr
            .contains("Project MCP servers not started: `project_server`")
            && run.stderr.contains("/mcp trust"),
        "{}",
        run.stderr
    );
}

#[test]
fn granting_an_unattended_run_tools_does_not_trust_project_servers() {
    let setup = Setup::new();

    let run = setup.run(&[
        "--print",
        "hello",
        "--allow-edit",
        "--allow-command",
        "echo *",
        "--allow-tool",
        "mcp__project_server__anything",
    ]);

    assert!(setup.started("user_server"), "{}", run.stderr);
    assert!(
        !setup.started("project_server"),
        "a grant for the run started an untrusted project server:\n{}",
        run.stderr
    );
}

#[test]
fn an_interactive_run_does_not_start_untrusted_project_servers() {
    for mode in ["confirm", "plan"] {
        let setup = Setup::new();

        // There is no terminal here, so the session ends once it is set up.
        let run = setup.run(&["--permission-mode", mode]);

        assert!(
            setup.started("user_server"),
            "{mode}: the run never reached server discovery:\n{}",
            run.stderr
        );
        assert!(
            !setup.started("project_server"),
            "{mode}: an untrusted project server was started:\n{}",
            run.stderr
        );
    }
}
