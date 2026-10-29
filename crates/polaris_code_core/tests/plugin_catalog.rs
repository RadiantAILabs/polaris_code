//! Drift guard for the plugin catalog in `src/docs/plugins.md`.
//!
//! Scans `src/` for `impl Plugin for X` declarations and asserts each plugin
//! name appears in the catalog. Adding a new plugin without listing it in the
//! catalog will fail this test.

use std::fs;
use std::path::{Path, PathBuf};

const CATALOG_PATH: &str = "src/docs/plugins.md";
const SCAN_DIR: &str = "src";

#[test]
fn every_exported_plugin_is_in_the_catalog() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let catalog_path = manifest_dir.join(CATALOG_PATH);
    let scan_dir = manifest_dir.join(SCAN_DIR);

    let catalog = fs::read_to_string(&catalog_path).unwrap_or_else(|err| {
        panic!(
            "failed to read plugin catalog at {}: {err}",
            catalog_path.display()
        )
    });

    let mut found = collect_plugin_impls(&scan_dir);
    found.sort();
    found.dedup();

    let missing: Vec<&String> = found
        .iter()
        .filter(|name| !catalog_mentions(&catalog, name))
        .collect();

    assert!(
        missing.is_empty(),
        "the following plugins are missing from {CATALOG_PATH}: {missing:?}\n\
         add an entry under the appropriate section. \
         every catalog entry should link to the plugin's rustdoc page."
    );
}

/// Walks `src/` and collects every plugin name that appears in a top-level
/// (non-indented) `impl Plugin for X` declaration. Indented matches are skipped
/// so test fixtures inside `#[cfg(test)] mod tests` blocks don't pollute the
/// inventory.
fn collect_plugin_impls(scan_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    visit_rs_files(scan_dir, &mut |path, contents| {
        if is_test_path(path) {
            return;
        }
        for line in contents.lines() {
            if let Some(name) = extract_plugin_name(line) {
                names.push(name);
            }
        }
    });
    names
}

fn visit_rs_files(dir: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            visit_rs_files(&path, visit);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs")
            && let Ok(contents) = fs::read_to_string(&path)
        {
            visit(&path, &contents);
        }
    }
}

fn is_test_path(path: &Path) -> bool {
    path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("tests") | Some("benches") | Some("examples")
        )
    })
}

/// Returns the plugin name iff `line` is `impl Plugin for <Name>` at column zero
/// (no leading whitespace, no doc-comment prefix). A generic *type* argument on
/// the target (`impl Plugin for Foo<Ready>`) is fine — the name stops at `<`.
/// A generic *impl* (`impl<T> Plugin for ...`) is skipped.
fn extract_plugin_name(line: &str) -> Option<String> {
    const PREFIX: &str = "impl Plugin for ";
    if !line.starts_with(PREFIX) {
        return None;
    }
    let rest = &line[PREFIX.len()..];
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    Some(rest[..end].to_string())
}

fn catalog_mentions(catalog: &str, name: &str) -> bool {
    // Match the name as a whole word so that `CodeToolsPlugin` doesn't
    // accidentally satisfy a request for `MyCodeToolsPlugin`.
    catalog.match_indices(name).any(|(i, _)| {
        let before = catalog[..i].chars().next_back();
        let after = catalog[i + name.len()..].chars().next();
        let boundary = |c: Option<char>| match c {
            None => true,
            Some(ch) => !(ch.is_alphanumeric() || ch == '_'),
        };
        boundary(before) && boundary(after)
    })
}

#[test]
fn extract_plugin_name_handles_common_shapes() {
    assert_eq!(
        extract_plugin_name("impl Plugin for CodeToolsPlugin {"),
        Some("CodeToolsPlugin".into())
    );
    assert_eq!(
        extract_plugin_name("impl Plugin for CodeAgentPlugin{"),
        Some("CodeAgentPlugin".into())
    );
    assert_eq!(extract_plugin_name("    impl Plugin for PluginA {"), None);
    assert_eq!(extract_plugin_name("/// impl Plugin for MyPlugin {"), None);
    assert_eq!(extract_plugin_name("impl<T> Plugin for Wrap<T> {"), None);
}

#[test]
fn catalog_mentions_respects_word_boundaries() {
    let catalog = "[`CodeAgentPlugin`](crate::CodeAgentPlugin)";
    assert!(catalog_mentions(catalog, "CodeAgentPlugin"));
    assert!(!catalog_mentions(catalog, "odeAgentPlugin"));
    assert!(!catalog_mentions(catalog, "CodeAgentPlug"));
}
