//! File-tool containment and schema contracts at the toolset boundary.

use std::path::Path;

use polaris::tools::{ToolContext, ToolError, Toolset};
use polaris_code::tools::{CallRoot, CodeTools, CodeToolsConfig};
use serde_json::{Value, json};

const FILE_TOOLS: [&str; 7] = [
    "list_files",
    "read_file",
    "read_span",
    "edit_file",
    "create_file",
    "search",
    "glob",
];
const ORIGINAL: &str = "CALL_ROOT_MARKER\n";

fn arguments(tool: &str, directory: &str) -> Value {
    let file = format!("{directory}/sample.txt");
    match tool {
        "read_file" => json!({"path": file}),
        "read_span" => json!({"path": file, "start": 1, "end": 1}),
        "edit_file" => {
            json!({"path": file, "old_string": "CALL_ROOT_MARKER", "new_string": "changed", "replace_all": false})
        }
        "create_file" => json!({"path": format!("{directory}/created.txt"), "content": ORIGINAL}),
        "list_files" => json!({"path": directory}),
        "search" => json!({"path": directory, "pattern": "MARKER"}),
        "glob" => json!({"path": directory, "pattern": "*.txt"}),
        _ => panic!("unknown file tool: {tool}"),
    }
}

fn populate(directory: &Path, marker: &str) {
    std::fs::create_dir_all(directory).unwrap();
    std::fs::write(directory.join("sample.txt"), marker).unwrap();
}

#[tokio::test]
async fn every_file_tool_refuses_paths_outside_the_call_root() {
    let server = tempfile::tempdir().unwrap();
    let root = server.path().join("call");
    let outside = server.path().join("outside");
    populate(&root, ORIGINAL);
    populate(&outside, ORIGINAL);
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("linked-outside")).unwrap();

    let tools = CodeTools::new(CodeToolsConfig::new(server.path())).tools();
    let context = ToolContext::new().with(CallRoot::new(&root));
    let mut paths = vec![
        ("parent traversal", "../outside".to_string()),
        ("absolute path", outside.to_str().unwrap().to_owned()),
    ];
    #[cfg(unix)]
    paths.push(("symlink", "linked-outside".to_string()));
    for name in FILE_TOOLS {
        let tool = tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .unwrap();
        for (kind, path) in &paths {
            let args = arguments(name, path);
            let Err(error) = tool.execute(args.clone(), &context).await else {
                panic!("{name} must refuse {kind}: {args}");
            };
            assert!(
                matches!(&error, ToolError::ExecutionError(reason) if reason.contains("escapes sandbox")),
                "{name}, {kind}, {args}: expected a containment refusal, got {error:?}"
            );
            assert_eq!(
                std::fs::read_to_string(outside.join("sample.txt")).unwrap(),
                ORIGINAL,
                "{name}, {kind}: refused edits leave the outside file intact"
            );
            assert!(
                !outside.join("created.txt").exists(),
                "{name}, {kind}: refused creations leave no outside file"
            );
        }
        tool.execute(arguments(name, "."), &context)
            .await
            .unwrap_or_else(|error| {
                panic!(
                    "{name}: the same valid arguments must succeed inside the call root: {error}"
                )
            });
    }
}

#[test]
fn call_root_is_absent_from_every_advertised_file_tool_schema() {
    let directory = tempfile::tempdir().unwrap();
    let tools = CodeTools::new(CodeToolsConfig::new(directory.path())).tools();
    for tool in tools {
        let definition = tool.definition();
        assert!(
            !definition.parameters.to_string().contains("call_root"),
            "{} must keep the context out of its schema: {}",
            definition.name,
            definition.parameters
        );
    }
}

#[tokio::test]
async fn json_call_root_cannot_override_the_invocation_context() {
    for name in FILE_TOOLS {
        let server = tempfile::tempdir().unwrap();
        let root = server.path().join("call");
        let outside = server.path().join("outside");
        populate(&root, ORIGINAL);
        populate(&outside, "OUTSIDE_MARKER\n");
        std::fs::write(root.join("call-only.txt"), "").unwrap();
        std::fs::write(outside.join("outside-only.txt"), "").unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(server.path())).tools();
        let tool = tools
            .iter()
            .find(|tool| tool.definition().name == name)
            .unwrap();
        let context = ToolContext::new().with(CallRoot::new(&root));
        let mut args = arguments(name, ".");
        args["call_root"] = json!(outside);
        match tool.execute(args, &context).await {
            Ok(value) => {
                let text = value.as_str().expect("file tools return text");
                let expected = if matches!(name, "list_files" | "glob") {
                    "call-only.txt"
                } else {
                    "CALL_ROOT_MARKER"
                };
                assert!(
                    text.contains(expected),
                    "{name}: a JSON root cannot change which folder is used: {text}"
                );
                assert!(
                    !text.contains("OUTSIDE_MARKER") && !text.contains("outside-only.txt"),
                    "{name}: outside content cannot be read via a JSON root: {text}"
                );
                if name == "edit_file" {
                    assert_eq!(
                        std::fs::read_to_string(root.join("sample.txt")).unwrap(),
                        "changed\n"
                    );
                }
                if name == "create_file" {
                    assert_eq!(
                        std::fs::read_to_string(root.join("created.txt")).unwrap(),
                        ORIGINAL
                    );
                }
            }
            Err(ToolError::ParameterError(_)) => {}
            Err(error) => {
                panic!("{name}: a JSON root must be ignored or rejected as a parameter: {error:?}")
            }
        }
        assert_eq!(
            std::fs::read_to_string(outside.join("sample.txt")).unwrap(),
            "OUTSIDE_MARKER\n",
            "{name}: a JSON root cannot redirect an edit"
        );
        assert!(
            !outside.join("created.txt").exists(),
            "{name}: a JSON root cannot redirect a creation"
        );
        let mut escape = arguments(name, outside.to_str().unwrap());
        escape["call_root"] = json!(outside);
        let error = tool
            .execute(escape, &context)
            .await
            .expect_err("a forged JSON root cannot authorize an outside path");
        assert!(
            matches!(&error, ToolError::ParameterError(_))
                || matches!(&error, ToolError::ExecutionError(reason) if reason.contains("escapes sandbox")),
            "{name}: expected parameter or containment refusal, got {error:?}"
        );
    }
}
