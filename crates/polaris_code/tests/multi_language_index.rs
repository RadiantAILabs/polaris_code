//! Consumer proof for the zero-setup multi-language code index.

use polaris::tools::ToolRegistry;
use polaris_code::{CodeGraphTools, CodeIndexer, CodeToolsConfig};
use polaris_ontrace::oxigraph::OxigraphStore;
use serde_json::{Value, json};

async fn call(registry: &ToolRegistry, name: &str, symbol: &str) -> String {
    let value = registry
        .execute(name, &json!({ "name": symbol }))
        .await
        .unwrap();
    match value {
        Value::String(value) => value,
        other => panic!("{name} returned a non-string result: {other}"),
    }
}

#[tokio::test]
async fn every_approved_non_rust_language_populates_the_frozen_navigation_tools() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("service.py"),
        "def py_helper():\n    return 1\n\ndef py_run():\n    return py_helper()\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("service.js"),
        "function js_helper() { return 1; }\nfunction js_run() { return js_helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("service.ts"),
        "function ts_helper(): number { return 1; }\nfunction ts_run() { return ts_helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("view.tsx"),
        "const TsxHelper = () => <div />;\nconst TsxRun = () => TsxHelper();\n",
    )
    .unwrap();

    let store = OxigraphStore::in_memory().unwrap();
    CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeGraphTools::new(store, CodeToolsConfig::new(dir.path())));

    for (definition, caller) in [
        ("py_helper", "py_run"),
        ("js_helper", "js_run"),
        ("ts_helper", "ts_run"),
        ("TsxHelper", "TsxRun"),
    ] {
        let found = call(&registry, "find_definition", definition).await;
        assert!(
            found.contains(definition.split('_').next().unwrap_or(definition))
                || found.contains("service")
                || found.contains("view"),
            "definition for {definition}: {found}"
        );
        assert!(
            found.contains("Function"),
            "definition for {definition}: {found}"
        );

        let callers = call(&registry, "callers_of", definition).await;
        assert!(
            callers.contains(caller),
            "callers of {definition}: {callers}"
        );

        let callees = call(&registry, "callees_of", caller).await;
        assert!(
            callees.contains(definition),
            "callees of {caller}: {callees}"
        );
    }
}
