//! Workspace-level guardrails for the external harness evaluation adapter.
//!
//! The adapter is Python by design: it drives the shipped `polaris-code`
//! binary from outside, exactly as the benchmark does. This Rust integration
//! target makes `cargo test --workspace` cover that adapter while also proving
//! benchmark identities did not leak into the shipped agent source.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("polaris_code_core remains two levels below the repository root")
        .to_path_buf()
}

fn rust_sources_below(path: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("source directory remains readable") {
        let path = entry.expect("source entry remains readable").path();
        if path.is_dir() {
            rust_sources_below(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn workspace_tests_execute_the_external_adapter_suite() {
    let root = repository_root();
    // Discovery starts at `evals` rather than one pack's tests, so the core's
    // dataset-neutral suite and every pack's suite run together.
    let output = Command::new("python3")
        .args(["-m", "unittest", "discover", "-s", "evals", "-t", "."])
        .current_dir(&root)
        .output()
        .expect("python3 is required to run the stdlib-only evaluation adapter");

    assert!(
        output.status.success(),
        "evaluation adapter tests failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    // A discovery that imported nothing also exits zero, which would make this
    // guard report green while covering no adapter at all.
    let report = String::from_utf8_lossy(&output.stderr);
    let ran: usize = report
        .lines()
        .find_map(|line| line.strip_prefix("Ran "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or_else(|| panic!("unittest reported no test count:\n{report}"));
    assert!(
        ran >= 150,
        "discovery found only {ran} adapter tests; a package probably failed to import",
    );
}

#[test]
fn benchmark_contracts_stay_outside_the_shipped_agent_source() {
    let source_root = repository_root().join("crates/polaris_code_core/src");
    let mut sources = Vec::new();
    rust_sources_below(&source_root, &mut sources);
    assert!(!sources.is_empty(), "the shipped source tree must exist");

    // `swebench` subsumes any pack directory or module name, so adding a second
    // SWE-bench pack cannot reintroduce the leak under a new spelling.
    let forbidden = ["SWE-bench", "swebench", "polaris-code-harness-eval"];
    for path in sources {
        let source = fs::read_to_string(&path).expect("Rust source remains UTF-8");
        for marker in forbidden {
            assert!(
                !source.contains(marker),
                "benchmark marker {marker:?} leaked into {}",
                path.display(),
            );
        }
    }
}
