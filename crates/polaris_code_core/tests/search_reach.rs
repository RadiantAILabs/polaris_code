//! `search` and `glob` reach, exercised the way the model reaches them.
//!
//! Every case here goes through the [`ToolRegistry`] with JSON arguments rather
//! than calling the inherent method, because half of what the tool delivers is
//! the *schema*: that `pattern` is the only required argument, that `path`,
//! `glob`, `literal`, `case_insensitive` and `include_ignored` may be omitted,
//! and that a bad value comes back as a diagnostic rather than an empty result.
//!
//! The fixture is a miniature repository: a root `.gitignore` that excludes a
//! vendored tree and `*.log`, a nested `.gitignore` inside `src/`, a `.git`
//! directory, and matches placed so that each dimension (pattern, path, glob,
//! ignore) can be isolated from the others.

use polaris::tools::{ToolError, ToolRegistry};
use polaris_code_core::{CodeTools, CodeToolsConfig};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// A repository-shaped tree. Returns the tempdir; the layout is:
///
/// ```text
/// .gitignore          vendor/  *.log
/// src/.gitignore      generated.rs
/// src/alpha.rs        fn alpha() / TODO(ana)
/// src/beta.rs         fn beta()  / TODO(bo)
/// src/generated.rs    fn alpha_generated()      (ignored: nested rule)
/// docs/notes.md       prose mentioning fn alpha
/// vendor/dep.rs       fn alpha_vendored()       (ignored: root rule)
/// debug.log           fn alpha_logged()         (ignored: root rule)
/// .git/HEAD           fn alpha_git()            (never walked)
/// ```
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for sub in ["src", "docs", "vendor", ".git"] {
        std::fs::create_dir_all(root.join(sub)).unwrap();
    }
    write(root, ".gitignore", "vendor/\n*.log\n");
    write(root, "src/.gitignore", "generated.rs\n");
    write(
        root,
        "src/alpha.rs",
        "fn alpha() {}\n// TODO(ana): tidy up\nlet pattern = value[0];\n",
    );
    write(root, "src/beta.rs", "fn beta() {}\n// TODO(bo): rename\n");
    write(root, "src/generated.rs", "fn alpha_generated() {}\n");
    write(root, "docs/notes.md", "The fn alpha entry point.\n");
    write(root, "vendor/dep.rs", "fn alpha_vendored() {}\n");
    write(root, "debug.log", "fn alpha_logged() {}\n");
    write(root, ".git/HEAD", "fn alpha_git() {}\n");
    dir
}

fn write(root: &Path, relative: &str, body: &str) {
    std::fs::write(root.join(relative), body).unwrap();
}

fn registry(root: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeTools::new(CodeToolsConfig::new(root)));
    registry
}

async fn call(registry: &ToolRegistry, name: &str, args: Value) -> String {
    match registry.execute(name, &args).await.unwrap() {
        Value::String(text) => text,
        other => panic!("{name} returned a non-string result: {other}"),
    }
}

async fn call_err(registry: &ToolRegistry, name: &str, args: Value) -> ToolError {
    registry
        .execute(name, &args)
        .await
        .expect_err("expected a diagnostic")
}

/// The hit paths in a `search` result, ignoring the trailing `…` summary lines.
fn hit_paths(result: &str) -> Vec<&str> {
    result
        .lines()
        .filter(|line| !line.starts_with('…'))
        .filter_map(|line| line.split(':').next())
        .collect()
}

#[tokio::test]
async fn a_pattern_finds_what_a_literal_cannot() {
    let dir = fixture();
    let registry = registry(dir.path());

    // One query, two symbols, neither of which is a substring of the other.
    let matched = call(
        &registry,
        "search",
        json!({ "pattern": r"fn (alpha|beta)\(\)" }),
    )
    .await;
    let paths = hit_paths(&matched);
    assert!(paths.contains(&"src/alpha.rs"), "{matched}");
    assert!(paths.contains(&"src/beta.rs"), "{matched}");

    // The same query as a literal is exactly what the old tool could express,
    // and it finds nothing — the regex is not a substring of any line.
    let literal = call(
        &registry,
        "search",
        json!({ "pattern": r"fn (alpha|beta)\(\)", "literal": true }),
    )
    .await;
    assert!(
        literal.starts_with("no matches"),
        "a literal must not match the regex it spells: {literal}"
    );

    // Character classes and anchors work too.
    let todos = call(
        &registry,
        "search",
        json!({ "pattern": r"^// TODO\(\w+\)" }),
    )
    .await;
    let paths = hit_paths(&todos);
    assert!(paths.contains(&"src/alpha.rs"), "{todos}");
    assert!(paths.contains(&"src/beta.rs"), "{todos}");
}

#[tokio::test]
async fn case_insensitivity_is_opt_in() {
    let dir = fixture();
    let registry = registry(dir.path());

    let sensitive = call(&registry, "search", json!({ "pattern": "FN ALPHA" })).await;
    assert!(sensitive.starts_with("no matches"), "{sensitive}");

    let folded = call(
        &registry,
        "search",
        json!({ "pattern": "FN ALPHA", "case_insensitive": true }),
    )
    .await;
    assert!(hit_paths(&folded).contains(&"src/alpha.rs"), "{folded}");
}

#[tokio::test]
async fn a_literal_with_metacharacters_still_matches_literally() {
    let dir = fixture();
    let registry = registry(dir.path());
    let needle = "value[0]";

    // As a regex, `value[0]` is a character class: it matches `value0`, which
    // is nowhere in the tree, and not the bracketed text that is.
    let as_regex = call(&registry, "search", json!({ "pattern": needle })).await;
    assert!(
        as_regex.starts_with("no matches"),
        "the class form must not match the bracketed text: {as_regex}"
    );

    let as_literal = call(
        &registry,
        "search",
        json!({ "pattern": needle, "literal": true }),
    )
    .await;
    assert!(
        as_literal.contains("src/alpha.rs:3: let pattern = value[0];"),
        "{as_literal}"
    );

    // And escaping by hand reaches the same place, so neither route is a trap.
    let escaped = call(&registry, "search", json!({ "pattern": r"value\[0\]" })).await;
    assert!(hit_paths(&escaped).contains(&"src/alpha.rs"), "{escaped}");
}

#[tokio::test]
async fn a_malformed_pattern_is_a_diagnostic_not_an_empty_result() {
    let dir = fixture();
    let registry = registry(dir.path());

    let err = call_err(&registry, "search", json!({ "pattern": "fn (alpha" }))
        .await
        .to_string();
    assert!(
        err.contains("not a valid regular expression"),
        "must name the fault: {err}"
    );
    assert!(
        err.contains("unclosed group"),
        "must say what was wrong: {err}"
    );
    assert!(
        err.contains("literal = true"),
        "must offer the way out: {err}"
    );

    let empty = call_err(&registry, "search", json!({ "pattern": "" }))
        .await
        .to_string();
    assert!(empty.contains("pattern is empty"), "{empty}");
}

#[tokio::test]
async fn a_path_restricts_the_search_to_one_subtree() {
    let dir = fixture();
    let registry = registry(dir.path());

    let everywhere = call(&registry, "search", json!({ "pattern": "alpha" })).await;
    let paths = hit_paths(&everywhere);
    assert!(paths.contains(&"src/alpha.rs"), "{everywhere}");
    assert!(paths.contains(&"docs/notes.md"), "{everywhere}");

    let scoped = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "path": "docs" }),
    )
    .await;
    let paths = hit_paths(&scoped);
    assert_eq!(paths, vec!["docs/notes.md"], "{scoped}");

    // A single file is a legitimate scope too.
    let one_file = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "path": "src/alpha.rs" }),
    )
    .await;
    assert_eq!(hit_paths(&one_file), vec!["src/alpha.rs"], "{one_file}");

    // Paths are reported relative to the working directory whatever the scope,
    // so a hit feeds straight back into `read_file`.
    assert!(scoped.starts_with("docs/notes.md:1:"), "{scoped}");
}

#[tokio::test]
async fn a_glob_restricts_the_search_to_a_set_of_files() {
    let dir = fixture();
    let registry = registry(dir.path());

    let rust_only = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "glob": "*.rs" }),
    )
    .await;
    let paths = hit_paths(&rust_only);
    assert!(paths.contains(&"src/alpha.rs"), "{rust_only}");
    assert!(
        !paths.contains(&"docs/notes.md"),
        "a markdown file is not *.rs: {rust_only}"
    );

    // An anchored glob is relative to `path`.
    let anchored = call(
        &registry,
        "search",
        json!({ "pattern": "fn", "glob": "src/*.rs" }),
    )
    .await;
    let paths = hit_paths(&anchored);
    assert!(paths.contains(&"src/alpha.rs"), "{anchored}");
    assert!(!paths.contains(&"docs/notes.md"), "{anchored}");

    // A glob that selects nothing says so, rather than reporting "no matches"
    // for a pattern that was never given a file to run against.
    let nothing = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "glob": "*.zig" }),
    )
    .await;
    assert!(nothing.contains("no files under"), "{nothing}");
    assert!(nothing.contains("*.zig"), "{nothing}");
}

#[tokio::test]
async fn ignored_files_are_skipped_by_default_and_searched_on_request() {
    let dir = fixture();
    let registry = registry(dir.path());

    let default = call(&registry, "search", json!({ "pattern": "alpha" })).await;
    let paths = hit_paths(&default);
    assert!(paths.contains(&"src/alpha.rs"), "{default}");
    for ignored in ["vendor/dep.rs", "debug.log", "src/generated.rs"] {
        assert!(
            !paths.contains(&ignored),
            "{ignored} is excluded by a .gitignore: {default}"
        );
    }
    assert!(
        !paths.iter().any(|p| p.starts_with(".git/")),
        "git's own storage is never searched: {default}"
    );

    let overridden = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "include_ignored": true }),
    )
    .await;
    let paths = hit_paths(&overridden);
    assert!(paths.contains(&"vendor/dep.rs"), "{overridden}");
    assert!(paths.contains(&"debug.log"), "{overridden}");
    assert!(
        paths.contains(&"src/generated.rs"),
        "a nested .gitignore is lifted too: {overridden}"
    );
    assert!(
        !paths.iter().any(|p| p.starts_with(".git/")),
        "the override lifts repository ignore rules, not .git: {overridden}"
    );
}

#[tokio::test]
async fn a_root_ignore_rule_still_applies_when_searching_a_subtree() {
    let dir = fixture();
    let registry = registry(dir.path());
    // `generated.rs` is ruled out by `src/.gitignore`; `vendor/` by the root's.
    // Starting the walk inside `src` must not lose either.
    let scoped = call(
        &registry,
        "search",
        json!({ "pattern": "alpha", "path": "src" }),
    )
    .await;
    assert_eq!(hit_paths(&scoped), vec!["src/alpha.rs"], "{scoped}");
}

#[tokio::test]
async fn results_are_bounded_and_say_so() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let body: String = (0..400).map(|n| format!("needle {n}\n")).collect();
    std::fs::write(root.join("many.txt"), body).unwrap();
    let registry = registry(root);

    let result = call(&registry, "search", json!({ "pattern": "needle" })).await;
    let hits = result.lines().filter(|l| !l.starts_with('…')).count();
    assert_eq!(hits, 200, "the match cap holds: {hits}");
    assert!(
        result.contains("stopped at 200 matches"),
        "a bounded result must say it was bounded: {}",
        result.lines().next_back().unwrap_or_default()
    );

    // An unbounded result reports its own shape instead.
    let small = call(&registry, "search", json!({ "pattern": "needle 39[0-9]" })).await;
    assert!(small.contains("10 match(es) in 1 of 1 file(s)"), "{small}");
}

#[tokio::test]
async fn an_over_long_matching_line_is_clipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("min.js"),
        format!("var x={};\n", "a".repeat(5000)),
    )
    .unwrap();
    let registry = registry(root);

    let result = call(&registry, "search", json!({ "pattern": "var x" })).await;
    assert!(result.contains("(line truncated)"), "{result}");
    assert!(result.len() < 1000, "{} bytes", result.len());
}

#[tokio::test]
async fn glob_finds_files_by_pattern() {
    let dir = fixture();
    let registry = registry(dir.path());

    let rust = call(&registry, "glob", json!({ "pattern": "*.rs" })).await;
    let listed: Vec<&str> = rust.lines().filter(|l| !l.starts_with('…')).collect();
    assert_eq!(listed, vec!["src/alpha.rs", "src/beta.rs"], "{rust}");

    let anchored = call(&registry, "glob", json!({ "pattern": "src/**/*.rs" })).await;
    let listed: Vec<&str> = anchored.lines().filter(|l| !l.starts_with('…')).collect();
    assert_eq!(listed, vec!["src/alpha.rs", "src/beta.rs"], "{anchored}");

    let scoped = call(
        &registry,
        "glob",
        json!({ "pattern": "*.md", "path": "docs" }),
    )
    .await;
    assert_eq!(scoped.lines().next().unwrap(), "docs/notes.md", "{scoped}");
}

#[tokio::test]
async fn glob_respects_ignore_rules_and_the_override() {
    let dir = fixture();
    let registry = registry(dir.path());

    let default = call(&registry, "glob", json!({ "pattern": "*.rs" })).await;
    assert!(!default.contains("vendor/dep.rs"), "{default}");
    assert!(!default.contains("generated.rs"), "{default}");

    let overridden = call(
        &registry,
        "glob",
        json!({ "pattern": "*.rs", "include_ignored": true }),
    )
    .await;
    assert!(overridden.contains("vendor/dep.rs"), "{overridden}");
    assert!(overridden.contains("src/generated.rs"), "{overridden}");
}

#[tokio::test]
async fn an_unusable_glob_is_a_diagnostic() {
    let dir = fixture();
    let registry = registry(dir.path());

    let err = call_err(&registry, "glob", json!({ "pattern": "" }))
        .await
        .to_string();
    assert!(err.contains("empty"), "{err}");

    let negated = call_err(
        &registry,
        "search",
        json!({"pattern": "fn", "glob": "!*.rs"}),
    )
    .await
    .to_string();
    assert!(negated.contains("'!'"), "{negated}");
    assert!(
        negated.contains("'*.rs'"),
        "must suggest the fix: {negated}"
    );

    let missing = call(&registry, "glob", json!({ "pattern": "*.zig" })).await;
    assert!(
        missing.contains("no files under"),
        "an empty result is a sentence, not a blank: {missing}"
    );
}

#[tokio::test]
async fn the_sandbox_still_holds() {
    let dir = fixture();
    let registry = registry(dir.path());

    for args in [
        json!({ "pattern": "alpha", "path": "../.." }),
        json!({ "pattern": "alpha", "path": "/etc" }),
    ] {
        let err = call_err(&registry, "search", args.clone())
            .await
            .to_string();
        assert!(err.contains("escapes sandbox"), "{args}: {err}");
    }

    let err = call_err(&registry, "glob", json!({ "pattern": "*", "path": "/etc" }))
        .await
        .to_string();
    assert!(err.contains("escapes sandbox"), "{err}");
}

#[tokio::test]
async fn the_advertised_schema_needs_only_a_pattern() {
    let dir = fixture();
    let registry = registry(dir.path());
    let definitions = registry.definitions();

    for (tool, optional) in [
        (
            "search",
            vec![
                "path",
                "glob",
                "literal",
                "case_insensitive",
                "include_ignored",
            ],
        ),
        ("glob", vec!["path", "include_ignored"]),
    ] {
        let definition = definitions
            .iter()
            .find(|d| d.name == tool)
            .unwrap_or_else(|| panic!("{tool} is not registered"));
        let properties = &definition.parameters["properties"];
        assert!(properties.get("pattern").is_some(), "{tool}: {properties}");
        for name in &optional {
            assert!(
                properties.get(name).is_some(),
                "{tool} is missing {name}: {properties}"
            );
        }
        let required: Vec<&str> = definition.parameters["required"]
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(required, vec!["pattern"], "{tool} requires: {required:?}");

        // The description has to carry the dialect, or a model guesses.
        let description = definition.description.to_lowercase();
        assert!(
            description.contains("gitignore"),
            "{tool} must name its glob dialect: {description}"
        );
    }

    let search = definitions.iter().find(|d| d.name == "search").unwrap();
    let description = search.description.to_lowercase();
    assert!(
        description.contains("regular expression"),
        "search must name its pattern dialect: {description}"
    );
}

/// The search the tool replaced: recurse from a directory, skip dotfiles and
/// `target`, read every remaining file whole, and test each line with
/// `contains`. Kept here so the timing claim is reproducible
/// and so the behavioural comparisons above are measured against the real
/// thing rather than a description of it.
fn legacy_search(root: &Path, query: &str) -> (usize, usize) {
    let mut hits = 0usize;
    let mut files = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || name == "target" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(content) = std::fs::read_to_string(&path) {
                files += 1;
                for line in content.lines() {
                    if line.contains(query) {
                        hits += 1;
                        if hits >= 200 {
                            return (hits, files);
                        }
                    }
                }
            }
        }
    }
    (hits, files)
}

/// Copy a real directory into the benchmark tree, skipping symlinks so the two
/// implementations see the same bytes (the walk does not follow links).
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let kind = entry.file_type().unwrap();
        let target = to.join(entry.file_name());
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            let _ = std::fs::copy(entry.path(), target);
        }
    }
}

/// File count and total bytes under `root`, for the benchmark's own report.
fn tree_size(root: &Path) -> (usize, u64) {
    let mut files = 0usize;
    let mut bytes = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    files += 1;
                    bytes += entry.metadata().map(|m| m.len()).unwrap_or_default();
                }
                _ => {}
            }
        }
    }
    (files, bytes)
}

/// Where the benchmark takes its working tree from: this checkout by default,
/// overridable with `POLARIS_CODE_SEARCH_BENCH_SOURCE`.
fn bench_source() -> PathBuf {
    std::env::var("POLARIS_CODE_SEARCH_BENCH_SOURCE").map_or_else(
        |_| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .expect("the crate sits two levels below the repository root")
                .to_path_buf()
        },
        PathBuf::from,
    )
}

/// Where the benchmark takes its bulk of ignored output from: this profile's
/// build directory by default, overridable with
/// `POLARIS_CODE_SEARCH_BENCH_BULK`. Missing is fine — the tree is just smaller.
fn bench_bulk() -> PathBuf {
    std::env::var("POLARIS_CODE_SEARCH_BENCH_BULK").map_or_else(
        |_| {
            let exe = std::env::current_exe().expect("the test binary has a path");
            exe.ancestors()
                .nth(2)
                .expect("the test binary sits in <profile>/deps/")
                .join("build")
        },
        PathBuf::from,
    )
}

/// A before/after timing on a realistic tree:
///
/// ```text
/// cargo test --test search_reach -- --ignored --nocapture bench
/// ```
///
/// The tree is assembled here rather than taken as-is so both implementations
/// measure the same thing: [`bench_source`] is copied in as the working tree,
/// [`bench_bulk`] is copied to `build/`, and a `.gitignore` excludes `build/` —
/// the shape a coding agent actually meets, where most of what is on disk is
/// output the repository has already said to ignore, under a name the old scan
/// did not hardcode.
#[tokio::test]
#[ignore = "timing benchmark; copies a large tree and reports numbers rather than asserting"]
async fn bench_search_against_the_implementation_it_replaces() {
    let tree = tempfile::tempdir().unwrap();
    let root = tree.path();
    copy_tree(&bench_source(), root);
    std::fs::write(root.join(".gitignore"), "build/\nnode_modules/\n*.log\n").unwrap();
    let bulk = bench_bulk();
    if bulk.is_dir() {
        copy_tree(&bulk, &root.join("build"));
    }

    let query = "CodeToolsConfig";
    let registry = registry(root);

    let started = std::time::Instant::now();
    let (legacy_hits, legacy_files) = legacy_search(root, query);
    let legacy = started.elapsed();

    let started = std::time::Instant::now();
    let current = call(
        &registry,
        "search",
        json!({ "pattern": query, "literal": true }),
    )
    .await;
    let current_elapsed = started.elapsed();

    let started = std::time::Instant::now();
    let scoped = call(
        &registry,
        "search",
        json!({ "pattern": query, "literal": true, "glob": "*.rs" }),
    )
    .await;
    let scoped_elapsed = started.elapsed();

    let count = |result: &str| {
        result
            .lines()
            .filter(|line| !line.starts_with('…'))
            .filter(|line| !line.starts_with("no "))
            .count()
    };
    let (files, bytes) = tree_size(root);
    println!("tree:    {files} files, {} MB", bytes / 1_000_000);
    println!("legacy:  {legacy:?} — {legacy_hits} hits over {legacy_files} files read");
    println!(
        "search:  {current_elapsed:?} — {} hits\n         {}",
        count(&current),
        current.lines().next_back().unwrap_or_default()
    );
    println!(
        "+glob:   {scoped_elapsed:?} — {} hits\n         {}",
        count(&scoped),
        scoped.lines().next_back().unwrap_or_default()
    );
}
