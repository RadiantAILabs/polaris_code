//! Bounded, deterministic test selection for lean edit batches.

use std::collections::BTreeSet;
use std::path::Path;

use polaris::shell::{ShellCancel, ShellExecutor};

const MAX_FILES: usize = 20_000;
const MAX_TARGETS: usize = 8;

fn files(root: &Path) -> Vec<String> {
    crate::tools::fileset::select_files(
        root,
        root,
        None,
        true,
        crate::tools::fileset::WalkLimits {
            max_files: MAX_FILES,
            max_file_bytes: 1 << 20,
        },
    )
    .files
    .into_iter()
    .filter_map(|p| {
        p.strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned())
    })
    .filter(|p| {
        !p.split('/')
            .any(|s| matches!(s, ".polaris" | "node_modules" | "target" | ".venv"))
    })
    .collect()
}

/// Metadata detects shell-created/modified files without reading their contents.
type FileSnapshot = std::collections::BTreeMap<String, (u64, Option<std::time::SystemTime>)>;

pub(super) async fn snapshot(root: &Path) -> Option<FileSnapshot> {
    let root = root.to_owned();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            files(&root)
                .into_iter()
                .filter(|p| {
                    Path::new(p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| {
                            matches!(
                                e,
                                "py" | "pyi"
                                    | "pyx"
                                    | "pxd"
                                    | "c"
                                    | "h"
                                    | "cc"
                                    | "cpp"
                                    | "rs"
                                    | "js"
                                    | "ts"
                                    | "tsx"
                            )
                        })
                })
                .filter_map(|p| {
                    let m = root.join(&p).metadata().ok()?;
                    Some((p, (m.len(), m.modified().ok())))
                })
                .collect()
        }),
    )
    .await
    .ok()?
    .ok()
}

fn test_file(path: &str) -> bool {
    let name = Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    (name == "tests.py"
        || name == "test.py"
        || name.starts_with("test_")
        || name.ends_with("_test.py"))
        && name.ends_with(".py")
}

fn local_file(root: &Path, path: &str) -> bool {
    root.join(path)
        .canonicalize()
        .is_ok_and(|p| p.starts_with(root) && p.is_file())
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A bounded test selection with the exact files whose result it covers.
struct Selection {
    command: String,
    targets: Vec<String>,
    pytest: bool,
}

/// The result kept for the model and, when safe, the cascade check.
pub(super) struct AutotestResult {
    pub summary: String,
    pub passed: bool,
    pub targets: Vec<String>,
    pub pytest: bool,
}

/// Prefer the project's runner, and never fall back to the entire suite.
fn selection(root: &Path, edited: &[String]) -> Option<Selection> {
    let root = root.canonicalize().ok()?;
    let edited: Vec<String> = edited
        .iter()
        .filter_map(|path| {
            root.join(path)
                .canonicalize()
                .ok()?
                .strip_prefix(&root)
                .ok()?
                .to_str()
                .map(str::to_owned)
        })
        .collect();
    let custom = ["tests/runtests.py", "runtests.py"]
        .into_iter()
        .find(|p| local_file(&root, p));
    let bin = local_file(&root, "bin/test");
    let pytest = local_file(&root, "pytest.ini")
        || local_file(&root, "conftest.py")
        || [
            ("pyproject.toml", "[tool.pytest"),
            ("setup.cfg", "[tool:pytest]"),
        ]
        .into_iter()
        .any(|(p, marker)| {
            local_file(&root, p)
                && std::fs::metadata(root.join(p)).is_ok_and(|m| m.len() < 65_536)
                && std::fs::read_to_string(root.join(p)).is_ok_and(|s| s.contains(marker))
        });
    if custom.is_none() && !bin && !pytest {
        return None;
    }
    let all = files(&root);
    let mut targets = BTreeSet::new();
    for edit in &edited {
        if test_file(edit) && local_file(&root, edit) {
            targets.insert(edit.clone());
            continue;
        }
        let path = Path::new(edit);
        let stem = path.file_stem()?.to_str()?;
        // Exact filename convention first; package directory is a secondary rule.
        let names = [format!("test_{stem}.py"), format!("{stem}_test.py")];
        let exact: Vec<_> = all
            .iter()
            .filter(|p| {
                names
                    .iter()
                    .any(|n| Path::new(p).file_name().is_some_and(|f| f == n.as_str()))
            })
            .collect();
        if !exact.is_empty() {
            targets.extend(exact.into_iter().cloned());
            continue;
        }
        let components: Vec<_> = path.iter().filter_map(|p| p.to_str()).collect();
        for component in components
            .iter()
            .rev()
            .skip(1)
            .filter(|p| !matches!(**p, "src" | "lib" | "tests" | "test"))
        {
            let matches: Vec<_> = all
                .iter()
                .filter(|p| test_file(p) && p.split('/').any(|s| s == *component))
                .collect();
            if !matches.is_empty() {
                targets.extend(matches.into_iter().cloned());
                break;
            }
        }
    }
    let mut targets: Vec<_> = targets
        .into_iter()
        .filter(|p| local_file(&root, p))
        .collect();
    let relevance = |p: &String| {
        edited
            .iter()
            .map(|e| {
                if e == p {
                    return usize::MAX;
                }
                e.split('/')
                    .filter(|part| !matches!(*part, "src" | "lib" | "tests" | "test"))
                    .filter(|part| p.split('/').any(|other| other == *part))
                    .count()
            })
            .max()
            .unwrap_or(0)
    };
    targets.sort_by_key(|p| (std::cmp::Reverse(relevance(p)), p.clone()));
    targets.truncate(MAX_TARGETS);
    if targets.is_empty() {
        return None;
    }
    if let Some(runner) = custom {
        let parent = Path::new(runner).parent()?.to_str()?;
        let labels: Option<Vec<_>> = targets
            .iter()
            .map(|p| {
                let relative = if parent.is_empty() {
                    p.as_str()
                } else {
                    p.strip_prefix(&format!("{parent}/"))?
                };
                let label = relative.strip_suffix(".py")?.replace('/', ".");
                label
                    .split('.')
                    .all(|s| {
                        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    })
                    .then(|| quote(&label))
            })
            .collect();
        Some(Selection {
            command: format!("python {runner} {}", labels?.join(" ")),
            targets,
            pytest: false,
        })
    } else {
        Some(Selection {
            command: format!(
                "{} {}",
                if bin {
                    "bin/test"
                } else {
                    "python -m pytest -q --tb=short --maxfail=1"
                },
                targets
                    .iter()
                    .map(|p| quote(&format!("./{p}")))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            targets,
            pytest: !bin,
        })
    }
}

/// One line naming how this project runs its own tests, for the model's first
/// message: the same runner [`selection`] would pick, or `None` when there is
/// no recognisable runner.
pub(super) fn runner_hint(root: &Path) -> Option<String> {
    let how = if let Some(runner) = ["tests/runtests.py", "runtests.py"]
        .into_iter()
        .find(|p| local_file(root, p))
    {
        format!(
            "`python {runner} <label>`, where a label is a test module path under `{}` written \
             with dots (for example `python {runner} app_name.test_module`); plain pytest and \
             importing `tests.` modules directly do not work here",
            Path::new(runner)
                .parent()
                .and_then(Path::to_str)
                .filter(|p| !p.is_empty())
                .unwrap_or(".")
        )
    } else if local_file(root, "bin/test") {
        "`python bin/test <path/to/test_file.py>` (pytest may not be installed)".to_owned()
    } else if local_file(root, "pytest.ini")
        || local_file(root, "conftest.py")
        || local_file(root, "setup.cfg")
        || local_file(root, "pyproject.toml")
    {
        "`python -m pytest -q <path/to/test_file.py>`".to_owned()
    } else {
        return None;
    };
    Some(format!(
        "\nThis project runs its tests with {how}. From run_python, call it with subprocess from \
         the workspace root, e.g. `subprocess.run([sys.executable, ...])`.\n"
    ))
}

/// Keep diagnostic lines as they stream past; head/tail alone can lose the first
/// failure in the middle of a noisy run. Each partial line and retained field is bounded.
#[derive(Default)]
struct Diagnostics {
    partial: [Vec<u8>; 2],
    first_failure: String,
    failure_lines: usize,
    counts: String,
    ran: Option<u64>,
}

impl Diagnostics {
    fn line(&mut self, line: &str) {
        let line = line.trim();
        if let Some(n) = line
            .strip_prefix("Ran ")
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
        {
            self.ran = Some(n);
        }
        if line.starts_with("OK") || line.starts_with("FAILED (") {
            if let Some(total) = self.ran {
                let number = |key: &str| -> u64 {
                    line.split_once(&format!("{key}="))
                        .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0)
                };
                let failed = number("failures");
                let errors = number("errors");
                let skipped = number("skipped") + number("expected failures");
                let unexpected = number("unexpected successes");
                self.counts = format!(
                    "passed={}, failed={failed}, errors={errors}, skipped/expected={skipped}, unexpected_successes={unexpected}",
                    total.saturating_sub(failed + errors + skipped + unexpected)
                );
            }
        } else if line
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit() || c == '=')
            && [" passed", " failed", " error", " skipped"]
                .iter()
                .any(|s| line.contains(s))
        {
            self.counts = line.chars().take(250).collect();
        }
        let failure = line.starts_with("FAIL:")
            || line.starts_with("ERROR:")
            || line.starts_with("FAILED ")
            || line.starts_with("E ")
            || line.contains("Error:")
            || line.starts_with("FAIL ");
        if (failure && self.first_failure.is_empty())
            || (self.failure_lines > 0 && self.failure_lines < 4)
        {
            if !self.first_failure.is_empty() {
                self.first_failure.push('\n');
            }
            self.first_failure.extend(
                line.chars()
                    .take(650_usize.saturating_sub(self.first_failure.chars().count())),
            );
            self.failure_lines += 1;
        }
    }

    fn feed(&mut self, stream: usize, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' || byte == b'\r' {
                let line = String::from_utf8_lossy(&std::mem::take(&mut self.partial[stream]))
                    .into_owned();
                self.line(&line);
            } else if self.partial[stream].len() < 1000 {
                self.partial[stream].push(byte);
            }
        }
    }

    fn render(&mut self, command: &str, status: &str) -> String {
        for stream in 0..2 {
            self.feed(stream, b"\n");
        }
        let counts = if self.counts.is_empty() {
            "pass/fail counts unavailable"
        } else {
            &self.counts
        };
        let text = format!(
            "\nAutotest (related subset): {}\n{status}\n{counts}\n{}",
            command.chars().take(350).collect::<String>(),
            self.first_failure
        );
        if text.chars().count() > 900 {
            format!("{}\n[clipped]", text.chars().take(889).collect::<String>())
        } else {
            text
        }
    }
}

#[derive(Clone, Default)]
struct Capture(std::sync::Arc<std::sync::Mutex<Diagnostics>>);

impl polaris::shell::ShellSink for Capture {
    fn on_event(&self, event: polaris::shell::ShellEvent) -> polaris::shell::ShellEventDelivery {
        if let polaris::shell::ShellEvent::Chunk { stream, bytes, .. } = event {
            let stream = match stream {
                polaris::shell::ShellStream::Stdout => 0,
                polaris::shell::ShellStream::Stderr => 1,
            };
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .feed(stream, &bytes);
        }
        polaris::shell::ShellEventDelivery::Accepted
    }
}

async fn run_selected(
    shell: &ShellExecutor,
    root: &Path,
    command: &str,
    cancel: ShellCancel,
    seconds: u64,
) -> (String, bool) {
    use polaris::shell::{ShellPermission, ShellRequest, ShellResponse, ShellSinkHandle};
    if !matches!(shell.check_permission(command), ShellPermission::Allow) {
        return (
            "\nAutotest not run: shell policy does not allow the detected runner unattended."
                .into(),
            false,
        );
    }
    let capture = Capture::default();
    let request = ShellRequest::new(command)
        .with_working_dir(root.to_owned())
        .with_timeout(seconds)
        .with_cancel(cancel)
        .with_sink(ShellSinkHandle::new(capture.clone()));
    // Executor timeout kills the process tree and bounds pipe draining. The
    // outer deadline also bounds waiting if the executor itself stalls.
    let (status, passed) = match tokio::time::timeout(
        std::time::Duration::from_secs(seconds + 5),
        shell.execute(request),
    )
    .await
    {
        Ok(Ok(ShellResponse::Executed(result))) => (
            format!(
                "exit={:?}; timeout={}; dropped_events={}",
                result.exit_code, result.timed_out, result.dropped_events
            ),
            result.exit_code == Some(0) && !result.timed_out,
        ),
        Ok(Ok(ShellResponse::ConfirmationRequired(_))) => {
            ("not run: confirmation required".into(), false)
        }
        Ok(Err(error)) => (
            format!(
                "runner error: {}",
                error.to_string().chars().take(200).collect::<String>()
            ),
            false,
        ),
        Err(_) => ("runner deadline exceeded; results incomplete".into(), false),
    };
    (
        capture
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .render(command, &status),
        passed,
    )
}

pub(super) async fn run(
    shell: Option<&ShellExecutor>,
    root: &Path,
    edited: &[String],
    cancel: ShellCancel,
) -> Option<AutotestResult> {
    let shell = shell?;
    let root = root.to_owned();
    let edits = edited.to_vec();
    let selection_root = root.clone();
    let selection = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || selection(&selection_root, &edits)),
    )
    .await
    .ok()?
    .ok()??;
    let (summary, passed) = run_selected(shell, &root, &selection.command, cancel, 120).await;
    Some(AutotestResult {
        summary,
        passed,
        targets: selection.targets,
        pytest: selection.pytest,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn runner_hint_names_the_project_runner() {
        let dir = tempfile::tempdir().unwrap();
        assert!(runner_hint(dir.path()).is_none());
        std::fs::create_dir_all(dir.path().join("bin")).unwrap();
        std::fs::write(dir.path().join("bin/test"), "").unwrap();
        assert!(runner_hint(dir.path()).unwrap().contains("python bin/test"));
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/runtests.py"), "").unwrap();
        let hint = runner_hint(dir.path()).unwrap();
        assert!(hint.contains("python tests/runtests.py <label>"));
        assert!(hint.contains("under `tests`"));
    }

    use super::*;

    fn fixture(paths: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, text) in paths {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    #[test]
    fn detects_project_runners_and_only_related_tests() {
        let dir = fixture(&[
            ("pytest.ini", ""),
            ("pkg/widgets.py", ""),
            ("tests/test_widgets.py", ""),
            ("tests/test_other.py", ""),
        ]);
        assert_eq!(
            selection(dir.path(), &["pkg/widgets.py".into()])
                .unwrap()
                .command,
            "python -m pytest -q --tb=short --maxfail=1 './tests/test_widgets.py'"
        );
        assert_eq!(
            selection(
                dir.path(),
                &[dir.path().join("pkg/widgets.py").display().to_string()]
            )
            .unwrap()
            .command,
            "python -m pytest -q --tb=short --maxfail=1 './tests/test_widgets.py'"
        );
        assert!(selection(dir.path(), &["pkg/unknown.py".into()]).is_none());
        std::fs::write(dir.path().join("tests/runtests.py"), "").unwrap();
        assert_eq!(
            selection(dir.path(), &["pkg/widgets.py".into()])
                .unwrap()
                .command,
            "python tests/runtests.py 'test_widgets'"
        );
        let unknown = fixture(&[("tests/test_widgets.py", "")]);
        assert!(selection(unknown.path(), &["tests/test_widgets.py".into()]).is_none());
        let bin = fixture(&[("bin/test", ""), ("tests/test_widgets.py", "")]);
        assert_eq!(
            selection(bin.path(), &["tests/test_widgets.py".into()])
                .unwrap()
                .command,
            "bin/test './tests/test_widgets.py'"
        );
    }

    #[tokio::test]
    async fn shell_snapshot_ignores_test_cache_output_but_finds_created_sources() {
        let dir = fixture(&[("tests/test_widget.py", "")]);
        let before = snapshot(dir.path()).await.unwrap();
        std::fs::create_dir(dir.path().join("__pycache__")).unwrap();
        std::fs::write(dir.path().join("__pycache__/widget.pyc"), "compiled output").unwrap();
        assert_eq!(before, snapshot(dir.path()).await.unwrap());
        std::fs::write(dir.path().join("tests/test_new.py"), "").unwrap();
        let after = snapshot(dir.path()).await.unwrap();
        assert_eq!(after.len(), before.len() + 1);
        assert!(after.contains_key("tests/test_new.py"));
    }

    #[tokio::test]
    async fn runner_timeout_permission_and_cancellation_are_bounded() {
        let dir = fixture(&[("hung.sh", "sleep 30\necho should-not-finish\n")]);
        let shell = ShellExecutor::new(
            polaris::shell::ShellConfig::new()
                .with_working_dir(dir.path())
                .with_allowed_dirs(vec![dir.path().to_owned()])
                .with_allowed_commands(vec!["sh *".into()])
                .disable_cache(),
        );
        let started = std::time::Instant::now();
        let output = run_selected(&shell, dir.path(), "sh hung.sh", ShellCancel::never(), 1).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(output.0.contains("timeout=true"), "{:?}", output);
        let output = run_selected(
            &shell,
            dir.path(),
            "touch forbidden",
            ShellCancel::never(),
            1,
        )
        .await;
        assert!(output.0.contains("not run"));
        assert!(!dir.path().join("forbidden").exists());
        let started = std::time::Instant::now();
        run_selected(
            &shell,
            dir.path(),
            "sh hung.sh",
            ShellCancel::from_fn(|| async {}),
            120,
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn streaming_summary_keeps_middle_failure_and_unittest_counts() {
        let mut d = Diagnostics::default();
        d.feed(0, "noise\n".repeat(10_000).as_bytes());
        for chunk in b"FAIL: test_widget\nAssertionError: first failure\n".chunks(3) {
            d.feed(1, chunk);
        }
        d.feed(0, "noise\n".repeat(10_000).as_bytes());
        d.feed(
            1,
            b"Ran 5 tests in 0.1s\nFAILED (failures=1, errors=1, skipped=1)\n",
        );
        let text = d.render("runner", "exit=1");
        assert!(text.contains("first failure"));
        assert!(text.contains("passed=2, failed=1, errors=1"), "{text}");
        assert!(text.chars().count() <= 900);
    }

    fn summary(command: &str, output: &str) -> String {
        let mut diagnostics = Diagnostics::default();
        diagnostics.feed(0, output.as_bytes());
        diagnostics.render(command, "exit=1")
    }

    #[test]
    fn summary_is_small_and_preserves_failure_and_counts() {
        let text = summary(
            "python -m pytest",
            &format!(
                "exit code: 1; timed out: false\nE AssertionError: first failure\n{}\n1 failed, 2 passed in 0.2s",
                "x".repeat(9000)
            ),
        );
        assert!(text.contains("1 failed, 2 passed"));
        assert!(text.contains("first failure"));
        assert!(text.chars().count() <= 900);
        assert!(summary("runner", "exit code: 0\nunknown output").contains("counts unavailable"));
    }
}
