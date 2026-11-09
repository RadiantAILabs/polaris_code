//! The sealed test check: run the test command over a copy of the working tree
//! that carries the sealed tests, never the agent's editable ones.
//!
//! The agent's own `run_command` runs tests in the real tree, so its result is
//! only as honest as the tree. This check copies the tree to a fresh directory,
//! puts the sealed protected files into the copy (and removes any test runner
//! configuration the seal did not hold), runs the command there with a clean
//! environment, and throws the copy away. What it reports is a recorded fact:
//! the exit code, the output's tail and digest, and the digest of the working
//! tree it ran over.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use polaris_cosmos::contracts::ContentDigest;
use serde::Serialize;
use tokio::io::AsyncReadExt;

use super::files::{Seal, copy_tree};

/// Most output bytes kept from a check; the tail is kept.
const OUTPUT_CAP: usize = 64 * 1024;

/// Most output bytes written into the record.
const RECORD_TAIL: usize = 4000;

/// Environment variables a check passes through from the host. Anything else
/// is dropped, so a check never sees credentials.
const ENV_ALLOW: &[&str] = &[
    "PATH",
    "CONDA_PREFIX",
    "CONDA_DEFAULT_ENV",
    "CONDA_SHLVL",
    "VIRTUAL_ENV",
    "PYTHONPATH",
    "LD_LIBRARY_PATH",
    "LANG",
    "LC_ALL",
];

/// What one sealed check recorded.
#[derive(Debug, Clone, Serialize)]
pub struct CheckResult {
    /// The command's exit code; `None` when it was killed or never started.
    pub exit_code: Option<i32>,
    /// Whether the command exited with code 0 before its time limit.
    pub passed: bool,
    /// Whether the time limit killed it.
    pub timed_out: bool,
    /// Wall time in milliseconds.
    pub duration_ms: u64,
    /// The last bytes of the command's combined output.
    pub output_tail: String,
    /// SHA-256 of the whole kept output.
    pub output_sha256: String,
    /// Why the check could not run, when it could not.
    pub error: Option<String>,
}

impl CheckResult {
    fn failed_to_run(error: String, started: Instant) -> Self {
        Self {
            exit_code: None,
            passed: false,
            timed_out: false,
            duration_ms: elapsed_ms(started),
            output_tail: String::new(),
            output_sha256: ContentDigest::sha256(b"").to_string(),
            error: Some(error),
        }
    }

    /// One line the agent can read: how the check ended.
    #[must_use]
    pub fn headline(&self) -> String {
        if let Some(error) = &self.error {
            format!("could not run: {error}")
        } else if self.timed_out {
            format!("timed out after {} ms", self.duration_ms)
        } else if self.passed {
            "passed (exit code 0)".to_string()
        } else {
            format!(
                "failed (exit code {})",
                self.exit_code
                    .map_or_else(|| "none".to_string(), |c| c.to_string())
            )
        }
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn tail(bytes: &[u8], keep: usize) -> String {
    let start = bytes.len().saturating_sub(keep);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

/// Run `command` over a sealed copy of `root`.
///
/// The command runs through `sh -c` in the copy, with standard error merged
/// into standard output, in its own process group so the time limit can end
/// every process it started.
pub async fn run_sealed_check(
    root: &Path,
    seal: &Seal,
    command: &str,
    limit: Duration,
) -> CheckResult {
    let started = Instant::now();
    let root_owned = root.to_path_buf();
    let seal_owned = seal.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let dir = tempfile::Builder::new()
            .prefix("polaris-check-")
            .tempdir()?;
        copy_tree(&root_owned, dir.path())?;
        seal_owned.apply_to_copy(dir.path())?;
        Ok::<_, std::io::Error>(dir)
    })
    .await;
    let dir = match prepared {
        Ok(Ok(dir)) => dir,
        Ok(Err(error)) => {
            return CheckResult::failed_to_run(format!("copying the tree: {error}"), started);
        }
        Err(error) => {
            return CheckResult::failed_to_run(format!("copying the tree: {error}"), started);
        }
    };

    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(format!("exec 2>&1\n{command}"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for name in ENV_ALLOW {
        if let Some(value) = std::env::var_os(name) {
            process.env(name, value);
        }
    }
    #[cfg(unix)]
    process.process_group(0);

    let mut child = match process.spawn() {
        Ok(child) => child,
        Err(error) => {
            return CheckResult::failed_to_run(format!("starting `sh`: {error}"), started);
        }
    };
    let pid = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        return CheckResult::failed_to_run("no output pipe".to_string(), started);
    };

    let reader = tokio::spawn(async move {
        let mut kept: Vec<u8> = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    kept.extend_from_slice(&buf[..n]);
                    if kept.len() > OUTPUT_CAP * 2 {
                        let drop = kept.len() - OUTPUT_CAP;
                        kept.drain(..drop);
                    }
                }
            }
        }
        kept
    });

    let (exit_code, timed_out) = match tokio::time::timeout(limit, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(_)) => (None, false),
        Err(_) => {
            kill_group(pid).await;
            let _ = child.kill().await;
            (None, true)
        }
    };
    // A descendant that outlived the shell would hold the pipe open. Only then
    // does a member of the group still exist, so only then is the group
    // signalled again; once the group is empty its id may belong to another
    // process.
    let output = match tokio::time::timeout(Duration::from_secs(5), reader).await {
        Ok(Ok(bytes)) => bytes,
        _ => {
            kill_group(pid).await;
            Vec::new()
        }
    };

    let kept = if output.len() > OUTPUT_CAP {
        &output[output.len() - OUTPUT_CAP..]
    } else {
        &output[..]
    };
    CheckResult {
        exit_code,
        passed: exit_code == Some(0) && !timed_out,
        timed_out,
        duration_ms: elapsed_ms(started),
        output_tail: tail(kept, RECORD_TAIL),
        output_sha256: ContentDigest::sha256(kept).to_string(),
        error: None,
    }
}

/// The arguments that make `kill` end the process group `pid` leads, or `None`
/// when `pid` is not a group this code may signal.
///
/// A group id of 0 or 1 (or no id) is never targeted: sent as a negative id,
/// those mean "this group", "init's group" or, through option parsing, "every
/// process this user can signal". The `--` is required for the same reason:
/// without it some `kill` programs read `-<pgid>` as an option, and an id that
/// starts with `1` becomes pid -1.
fn kill_group_arguments(pid: Option<u32>) -> Option<[String; 3]> {
    let pid = pid.filter(|pid| *pid > 1)?;
    Some(["-KILL".to_string(), "--".to_string(), format!("-{pid}")])
}

#[cfg(unix)]
async fn kill_group(pid: Option<u32>) {
    if let Some(arguments) = kill_group_arguments(pid) {
        let _ = tokio::process::Command::new("kill")
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
}

#[cfg(not(unix))]
async fn kill_group(_pid: Option<u32>) {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::rules::files::ProtectedSet;

    fn setup(test_body: &str) -> (tempfile::TempDir, Seal) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("solution.py"), "VALUE = 1\n").unwrap();
        std::fs::write(dir.path().join("test.py"), test_body).unwrap();
        let protected = ProtectedSet::new(dir.path(), &[PathBuf::from("test.py")]).unwrap();
        let seal = Seal::take(&protected).unwrap();
        (dir, seal)
    }

    const TEST_BODY: &str =
        "import solution\nassert solution.VALUE == 1, 'wrong value'\nprint('ok')\n";

    #[test]
    fn the_group_kill_never_names_a_group_that_could_mean_everything() {
        for unsafe_id in [None, Some(0), Some(1)] {
            assert_eq!(kill_group_arguments(unsafe_id), None, "{unsafe_id:?}");
        }
        for id in [2_u32, 10, 12345, 4_000_000] {
            let arguments = kill_group_arguments(Some(id)).unwrap();
            assert_eq!(
                arguments,
                ["-KILL".to_string(), "--".to_string(), format!("-{id}")],
                "the id must come after `--`, or `kill` may read it as an option"
            );
        }
    }

    #[tokio::test]
    async fn a_passing_command_is_recorded_as_a_pass() {
        let (dir, seal) = setup(TEST_BODY);
        let result = run_sealed_check(
            dir.path(),
            &seal,
            "python3 test.py",
            Duration::from_secs(30),
        )
        .await;
        assert!(result.passed, "{result:?}");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output_tail.contains("ok"));
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn the_check_runs_the_sealed_test_not_the_edited_one() {
        let (dir, seal) = setup(TEST_BODY);
        // The agent weakens the test and changes the answer to match.
        std::fs::write(dir.path().join("test.py"), "print('weakened')\n").unwrap();
        std::fs::write(dir.path().join("solution.py"), "VALUE = 2\n").unwrap();
        let result = run_sealed_check(
            dir.path(),
            &seal,
            "python3 test.py",
            Duration::from_secs(30),
        )
        .await;
        assert!(
            !result.passed,
            "the sealed test wants VALUE == 1: {result:?}"
        );
        assert_eq!(result.exit_code, Some(1));
        assert!(result.output_tail.contains("wrong value"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.py")).unwrap(),
            "print('weakened')\n",
            "the check never writes to the working tree"
        );
    }

    #[tokio::test]
    async fn a_planted_conftest_does_not_reach_the_check() {
        let (dir, seal) = setup("import solution\nassert solution.VALUE == 2\n");
        std::fs::write(dir.path().join("conftest.py"), "import sys\nsys.exit(0)\n").unwrap();
        // Whatever the runner is, the planted file is gone from the copy.
        let result = run_sealed_check(
            dir.path(),
            &seal,
            "test -e conftest.py && exit 0; python3 test.py",
            Duration::from_secs(30),
        )
        .await;
        assert!(!result.passed, "{result:?}");
    }

    #[tokio::test]
    async fn the_time_limit_ends_the_command_and_is_recorded() {
        let (dir, seal) = setup(TEST_BODY);
        let result =
            run_sealed_check(dir.path(), &seal, "sleep 30", Duration::from_millis(300)).await;
        assert!(result.timed_out);
        assert!(!result.passed);
        assert!(result.duration_ms < 10_000);
    }

    #[tokio::test]
    async fn the_check_environment_has_no_credentials() {
        let (dir, seal) = setup(TEST_BODY);
        // `env` lists what the child sees; a variable the host has but the
        // allow-list lacks must not be in it.
        let result = run_sealed_check(dir.path(), &seal, "env", Duration::from_secs(30)).await;
        assert!(result.passed, "{result:?}");
        assert!(!result.output_tail.contains("AWS_"));
        assert!(!result.output_tail.contains("ANTHROPIC"));
    }

    #[tokio::test]
    async fn standard_error_is_part_of_the_recorded_output() {
        let (dir, seal) = setup(TEST_BODY);
        let result = run_sealed_check(
            dir.path(),
            &seal,
            "echo to-stderr 1>&2; exit 3",
            Duration::from_secs(30),
        )
        .await;
        assert_eq!(result.exit_code, Some(3));
        assert!(result.output_tail.contains("to-stderr"));
        assert!(result.headline().contains("exit code 3"));
    }
}
