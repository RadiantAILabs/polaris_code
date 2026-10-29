//! Semantic composition proof for the permission modes and the
//! multi-language index.
//!
//! Each feature's own suite exercises one half in isolation:
//! `tests/permission_modes.rs` resolves modes against *hand-supplied* registry
//! defaults and never registers a tool, and `tests/multi_language_index.rs`
//! drives the indexer's tools through a registry that has no permission policy
//! over it. Neither can observe the seam where the two features actually meet.
//!
//! That seam is `execute_tools` in `src/agents/code_agent.rs`. For every call it
//! resolves `policy.permission.resolve(name, tool_registry.permission(name))`
//! and skips execution on `Deny`; a non-denied `edit_file` is the only thing
//! that pushes onto `edited`, and `edited` is what the post-loop
//! `CodeIndexer::index_paths` re-index consumes. So the permission surface is
//! the upstream control on the multi-language indexer, and the indexer's own
//! tools are subject to that surface.
//!
//! These tests reproduce that gate against the *real* registry defaults the
//! merged composition root installs, rather than synthetic ones.

use polaris::tools::{ToolPermission, ToolRegistry};
use polaris_code_core::agents::{PermissionMode, UnattendedGrants};
use polaris_code_core::tools::FILE_WRITE_TOOLS;
use polaris_code_core::{CodeGraphTools, CodeIndexer, CodeTools, CodeToolsConfig};
use polaris_ontrace::oxigraph::OxigraphStore;
use serde_json::{Value, json};

/// Every navigation tool the multi-language indexer populates.
const INDEXER_TOOLS: [&str; 7] = [
    "find_definition",
    "find_references",
    "outline",
    "read_symbol",
    "callers_of",
    "callees_of",
    "impls_of_trait",
];

/// The mixed-language repository both features have to agree about: a non-Rust
/// file only the multi-language index can index, plus a Rust file that
/// worked before it.
fn write_repo(dir: &std::path::Path) {
    std::fs::write(
        dir.join("service.py"),
        "def py_helper():\n    return 1\n\ndef py_run():\n    return py_helper()\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("service.ts"),
        "function ts_helper(): number { return 1; }\nfunction ts_run() { return ts_helper(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.rs"),
        "pub fn rs_helper() -> u32 { 1 }\npub fn rs_run() -> u32 { rs_helper() }\n",
    )
    .unwrap();
}

/// The merged composition root's tool surface: `CodeToolsPlugin` registers
/// `CodeTools` and narrows its mutating tools (`edit_file`, `create_file`) to
/// `Confirm` (`src/tools/code.rs`), and `CodeGraphPlugin` registers the
/// indexer's `CodeGraphTools` (`src/tools/code_graph.rs`). Built here without
/// a `Server` so the test owns no lifecycle, but with the same two
/// registrations and the same overrides.
fn merged_registry(store: OxigraphStore, config: CodeToolsConfig) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeTools::new(config.clone()));
    for tool in FILE_WRITE_TOOLS {
        registry
            .set_permission(tool, ToolPermission::Confirm)
            .unwrap_or_else(|_| panic!("{tool} should be registered by CodeTools toolset"));
    }
    registry.register_toolset(CodeGraphTools::new(store, config));
    registry
}

/// The gate `execute_tools` applies, in the same order: registry default (or
/// `Deny` when unregistered), then the mode's policy resolution.
fn gate(registry: &ToolRegistry, mode: &PermissionMode, tool: &str) -> ToolPermission {
    let registry_default = registry.permission(tool).unwrap_or(ToolPermission::Deny);
    mode.permission_policy().resolve(tool, registry_default)
}

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

fn modes() -> Vec<(&'static str, PermissionMode)> {
    vec![
        ("confirm", PermissionMode::Confirm),
        ("auto-edit", PermissionMode::AutoEdit),
        ("plan", PermissionMode::Plan),
        (
            "unattended",
            PermissionMode::Unattended(UnattendedGrants::default()),
        ),
    ]
}

/// The permission surface governs the indexer's tools, and admits them in every
/// mode — including fail-closed `Unattended`, where any tool left at `Confirm`
/// collapses to `Deny`. A merge that let one of the seven navigation tools
/// carry a confirming default would silently disable code navigation for
/// unattended runs while both features' own suites still passed.
#[tokio::test]
async fn the_permission_surface_governs_and_admits_every_indexer_tool() {
    let dir = tempfile::tempdir().unwrap();
    write_repo(dir.path());
    let store = OxigraphStore::in_memory().unwrap();
    CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
    let registry = merged_registry(store, CodeToolsConfig::new(dir.path()));

    for tool in INDEXER_TOOLS {
        assert!(
            registry.permission(tool).is_some(),
            "{tool} is not registered in the composed registry, so no policy governs it"
        );
        for (label, mode) in modes() {
            assert_eq!(
                gate(&registry, &mode, tool),
                ToolPermission::Allow,
                "{label} mode must leave the indexer's read-only {tool} available"
            );
        }
    }

    // The gate is discriminating, not uniformly permissive: the mutating tools
    // that feed the indexer are resolved from the same surface, per mode, and
    // as one — a creation is gated exactly as an edit.
    for (label, expected) in [
        ("confirm", ToolPermission::Confirm),
        ("auto-edit", ToolPermission::Allow),
        ("plan", ToolPermission::Deny),
        ("unattended", ToolPermission::Deny),
    ] {
        let mode = modes()
            .into_iter()
            .find(|(name, _)| *name == label)
            .map(|(_, mode)| mode)
            .unwrap();
        for tool in FILE_WRITE_TOOLS {
            assert_eq!(
                gate(&registry, &mode, tool),
                expected,
                "{label} mode must resolve {tool} to {expected:?}"
            );
        }
    }
}

/// A denied `edit_file` never reaches the indexer. `execute_tools` pushes onto
/// `edited` only inside the non-denied branch, so under `Plan` the post-loop
/// `index_paths` re-index has nothing to do — the source is untouched and the
/// multi-language facts still describe the original file.
#[tokio::test]
async fn plan_mode_denies_the_edit_that_would_refresh_the_multi_language_index() {
    let dir = tempfile::tempdir().unwrap();
    write_repo(dir.path());
    let store = OxigraphStore::in_memory().unwrap();
    CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
    let registry = merged_registry(store.clone(), CodeToolsConfig::new(dir.path()));

    let mode = PermissionMode::Plan;
    assert_eq!(gate(&registry, &mode, "edit_file"), ToolPermission::Deny);

    // Mirror `execute_tools`: on `Deny` the registry is never asked to execute
    // and nothing is appended to `edited`.
    let mut edited: Vec<std::path::PathBuf> = Vec::new();
    if gate(&registry, &mode, "edit_file") != ToolPermission::Deny {
        registry
            .execute(
                "edit_file",
                &json!({
                    "path": "service.py",
                    "old_string": "py_helper",
                    "new_string": "py_renamed",
                    "replace_all": true
                }),
            )
            .await
            .unwrap();
        edited.push(dir.path().join("service.py"));
    }

    assert!(
        edited.is_empty(),
        "a denied edit must not enqueue a re-index"
    );
    let source = std::fs::read_to_string(dir.path().join("service.py")).unwrap();
    assert!(
        source.contains("py_helper") && !source.contains("py_renamed"),
        "plan mode must leave the source unmodified: {source}"
    );
    let found = call(&registry, "find_definition", "py_helper").await;
    assert!(
        found.contains("service.py"),
        "the pre-existing Python facts must survive a denied edit: {found}"
    );
    let renamed = call(&registry, "find_definition", "py_renamed").await;
    assert!(
        renamed.contains("no definition"),
        "no re-index ran, so the renamed symbol cannot exist: {renamed}"
    );
}

/// When the surface grants the edit, the multi-language indexer runs behind it
/// and still behaves as its own suite requires: the re-index replaces the
/// edited non-Rust file's facts, through the frozen navigation tools. Covers
/// both granting modes — `AutoEdit`, and `Unattended` carrying an explicit
/// `edit_file` grant.
#[tokio::test]
async fn a_granted_edit_still_drives_the_multi_language_reindex() {
    for mode in [
        PermissionMode::AutoEdit,
        PermissionMode::Unattended(UnattendedGrants {
            edit_file: true,
            run_command_patterns: Vec::new(),
        }),
    ] {
        let dir = tempfile::tempdir().unwrap();
        write_repo(dir.path());
        let store = OxigraphStore::in_memory().unwrap();
        CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
        let registry = merged_registry(store.clone(), CodeToolsConfig::new(dir.path()));

        assert_eq!(
            gate(&registry, &mode, "edit_file"),
            ToolPermission::Allow,
            "{} must grant edit_file",
            mode.permission_policy().mode_label()
        );

        // Mirror `execute_tools`: execute, then enqueue the edited path.
        let mut edited: Vec<std::path::PathBuf> = Vec::new();
        registry
            .execute(
                "edit_file",
                &json!({
                    "path": "service.py",
                    "old_string": "py_helper",
                    "new_string": "py_renamed",
                    "replace_all": true
                }),
            )
            .await
            .unwrap();
        edited.push(dir.path().join("service.py"));

        // ...and the post-loop incremental re-index the agent performs.
        CodeIndexer::index_paths(&store, dir.path(), &edited)
            .await
            .unwrap();

        let renamed = call(&registry, "find_definition", "py_renamed").await;
        assert!(
            renamed.contains("service.py") && renamed.contains("Function"),
            "the re-index must publish the renamed Python function: {renamed}"
        );
        let stale = call(&registry, "find_definition", "py_helper").await;
        assert!(
            stale.contains("no definition"),
            "the re-index must retract the replaced Python function: {stale}"
        );
        let callers = call(&registry, "callers_of", "py_renamed").await;
        assert!(
            callers.contains("py_run"),
            "call-graph facts must survive the gated re-index: {callers}"
        );

        // The untouched languages are unaffected by the gated re-index.
        let ts = call(&registry, "find_definition", "ts_helper").await;
        assert!(ts.contains("service.ts"), "TypeScript facts lost: {ts}");
        let rs = call(&registry, "find_definition", "rs_helper").await;
        assert!(rs.contains("lib.rs"), "Rust facts lost: {rs}");
    }
}
