//! The file tools against a fixed list of calls and what they must do.
//!
//! `tests/vectors/file_tool_vectors.json` holds calls to `edit_file` and
//! `create_file`, each with the files it sees, whether it must succeed, the text
//! the model is given, and the files it must change or create. Another program
//! that applies these calls can run the same list to show it agrees with the
//! tools, so the list is the written form of how the tools apply a call.
//!
//! Each call runs through the tool registry the agent uses, on real files in a
//! new folder.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use polaris::tools::{ToolContext, ToolRegistry};
use serde::Deserialize;
use serde_json::Value;

use super::call_arguments::failure_text;
use super::{CodeTools, CodeToolsConfig};

#[derive(Deserialize)]
struct Vectors {
    vectors: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    id: String,
    tool: String,
    arguments: Value,
    files: BTreeMap<String, String>,
    expected: Expected,
}

#[derive(Deserialize)]
struct Expected {
    ok: bool,
    text: String,
    changed_files: BTreeMap<String, String>,
}

fn write_tree(root: &Path, files: &BTreeMap<String, String>) {
    for (relative, text) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

fn read_tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, into: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, into);
            } else if let Ok(text) = fs::read_to_string(&path) {
                let relative = path.strip_prefix(root).unwrap();
                into.insert(relative.to_string_lossy().replace('\\', "/"), text);
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

#[tokio::test]
async fn every_call_in_the_list_does_what_the_list_says() {
    let vectors: Vectors =
        serde_json::from_str(include_str!("../../tests/vectors/file_tool_vectors.json")).unwrap();
    assert!(vectors.vectors.len() >= 40, "the list was cut short");

    let mut disagreements = Vec::new();
    for vector in &vectors.vectors {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_tree(root, &vector.files);

        let mut registry = ToolRegistry::new();
        registry.register_toolset(CodeTools::new(CodeToolsConfig::new(root)));
        let outcome = registry
            .execute_with(&vector.tool, &vector.arguments, &ToolContext::new())
            .await;
        let (ok, text) = match outcome {
            Ok(value) => (
                true,
                value
                    .as_str()
                    .map_or_else(|| value.to_string(), String::from),
            ),
            Err(error) => (false, failure_text(&vector.tool, &vector.arguments, &error)),
        };
        let changed_files: BTreeMap<String, String> = read_tree(root)
            .into_iter()
            .filter(|(path, text)| vector.files.get(path) != Some(text))
            .collect();

        if ok != vector.expected.ok
            || text != vector.expected.text
            || changed_files != vector.expected.changed_files
        {
            disagreements.push(format!(
                "{}: ok {ok} (want {}), changed {:?} (want {:?}), text {text:?} (want {:?})",
                vector.id,
                vector.expected.ok,
                changed_files.keys().collect::<Vec<_>>(),
                vector.expected.changed_files.keys().collect::<Vec<_>>(),
                vector.expected.text,
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} call(s) differ from the list:\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}
