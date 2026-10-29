//! What the code graph is allowed to claim about a tree it may not have
//! finished reading, or may have read before something else changed it.
//!
//! The navigation tools are the substrate the rest of the architecture is
//! justified by, and a graph answer is only worth having if a reader can tell
//! the three cases apart:
//!
//! 1. **the index has not finished** — the graph cannot say yet;
//! 2. **the index is current and the symbol is absent** — the graph says no;
//! 3. **the index is behind the tree** — the graph must not answer at all
//!    until it has caught up on what the answer depends on.
//!
//! Before this suite the tools collapsed all three into case 2: a query issued
//! against a cold index returned `no definition found`, and a file rewritten by
//! anything other than `edit_file` (a `git checkout`, a code generator, the
//! agent's own `run_command`) kept answering from the version that was indexed.
//! Both readings are indistinguishable from the truth, which is what makes them
//! expensive — a model that reads "no definition found" goes and writes the
//! definition that already exists.
//!
//! Every assertion here is about what a *caller* can distinguish, never about
//! the wording of a particular sentence. The tools are reached the way the
//! agent reaches them: through the registry, by name, with JSON arguments.

use polaris::tools::ToolRegistry;
use polaris_code::{CodeGraphTools, CodeIndexer, CodeToolsConfig, code_index_state};
use polaris_ontrace::oxigraph::OxigraphStore;
use serde_json::{Value, json};

/// The negative answer, as the tools phrase it. Several tests assert this
/// string is *absent* when the graph is not entitled to a negative.
const NEGATIVE: &str = "no definition found";

/// Write `files` under a fresh tempdir.
fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (path, src) in files {
        let path = dir.path().join(path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, src).unwrap();
    }
    dir
}

fn graph_registry(store: OxigraphStore, root: &std::path::Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register_toolset(CodeGraphTools::new(store, CodeToolsConfig::new(root)));
    registry
}

async fn ask(registry: &ToolRegistry, tool: &str, name: &str) -> String {
    match registry
        .execute(tool, &json!({ "name": name }))
        .await
        .unwrap()
    {
        Value::String(text) => text,
        other => panic!("{tool} returned a non-string result: {other}"),
    }
}

/// Index `dir` and return a registry of navigation tools over it.
async fn indexed(dir: &std::path::Path) -> ToolRegistry {
    let store = OxigraphStore::in_memory().unwrap();
    CodeIndexer::index_repo(&store, dir).await.unwrap();
    graph_registry(store, dir)
}

/// A tree big enough that the boot index is still running when the first query
/// arrives — the shape of a real repository at startup.
fn wide_tree(files: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..files {
        let body = (0..20)
            .map(|j| format!("pub fn filler_{i}_{j}() -> u32 {{ {j} }}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join(format!("m{i}.rs")), body).unwrap();
    }
    std::fs::write(
        dir.path().join("zzz_target.rs"),
        "pub fn indexed_late() -> u32 { 7 }\n",
    )
    .unwrap();
    dir
}

/// **Readiness.** A query that races the boot index must never come back as a
/// negative. The caller may be told to wait, or given the real answer once the
/// index lands, but "this symbol does not exist" is a claim the graph has not
/// earned while it is still reading the tree.
#[tokio::test]
async fn a_query_racing_the_boot_index_is_never_a_negative_answer() {
    let dir = wide_tree(500);
    let store = OxigraphStore::in_memory().unwrap();
    let registry = graph_registry(store.clone(), dir.path());

    let indexing = {
        let store = store.clone();
        let root = dir.path().to_path_buf();
        tokio::spawn(async move { CodeIndexer::index_repo(&store, &root).await })
    };

    let mut answers = 0_usize;
    while !indexing.is_finished() {
        let out = ask(&registry, "find_definition", "indexed_late").await;
        answers += 1;
        assert!(
            !out.contains(NEGATIVE),
            "a query issued while the index is still building answered as if the \
             symbol did not exist; the caller cannot tell 'not yet' from 'not there'. \
             got: {out}"
        );
        tokio::task::yield_now().await;
    }
    indexing.await.unwrap().unwrap();
    assert!(answers > 0, "the race never happened; widen the tree");

    // And once it is ready the same question has the ordinary answer.
    let out = ask(&registry, "find_definition", "indexed_late").await;
    assert!(out.contains("zzz_target.rs"), "got: {out}");
}

/// **Readiness is distinguishable from absence.** The two cases must not read
/// the same, or the gate above is worthless.
#[tokio::test]
async fn not_ready_and_genuinely_absent_do_not_read_alike() {
    let dir = wide_tree(500);
    let store = OxigraphStore::in_memory().unwrap();
    let registry = graph_registry(store.clone(), dir.path());

    let indexing = {
        let store = store.clone();
        let root = dir.path().to_path_buf();
        tokio::spawn(async move { CodeIndexer::index_repo(&store, &root).await })
    };
    let mut pending = None;
    while !indexing.is_finished() {
        pending = Some(ask(&registry, "find_definition", "no_such_symbol_anywhere").await);
        tokio::task::yield_now().await;
    }
    indexing.await.unwrap().unwrap();
    let pending = pending.expect("the race never happened; widen the tree");
    let absent = ask(&registry, "find_definition", "no_such_symbol_anywhere").await;

    assert_ne!(
        pending, absent,
        "'still indexing' and 'no such symbol' returned the same text"
    );
    assert!(
        absent.contains(NEGATIVE),
        "a ready index owes a plain negative: {absent}"
    );
}

/// **Freshness, the `run_command` case.** Something other than `edit_file`
/// rewrites a file — `sed -i`, `cargo fmt`, a generator, a `git checkout`. The
/// graph must not keep answering from the version it read.
#[tokio::test]
async fn a_rewrite_outside_edit_file_does_not_produce_a_confidently_wrong_answer() {
    let dir = tree(&[(
        "lib.rs",
        "pub fn alpha() -> u32 {\n    1\n}\n\npub fn keeper() -> u32 {\n    2\n}\n",
    )]);
    let registry = indexed(dir.path()).await;
    assert!(
        ask(&registry, "find_definition", "alpha")
            .await
            .contains("lib.rs"),
        "fixture precondition"
    );

    // The `run_command` case: a rename performed entirely outside the toolset.
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn keeper() -> u32 {\n    2\n}\n\npub fn beta() -> u32 {\n    1\n}\n",
    )
    .unwrap();

    let gone = ask(&registry, "find_definition", "alpha").await;
    assert!(
        !gone.contains("lib.rs\t"),
        "the graph pointed at a definition that no longer exists: {gone}"
    );
    let added = ask(&registry, "find_definition", "beta").await;
    assert!(
        added.contains("lib.rs"),
        "the graph never saw the symbol the rewrite introduced: {added}"
    );
}

/// **Freshness, moved spans.** The sharpest form of the same defect:
/// `read_symbol` resolves a span from the index and then reads live bytes, so a
/// stale span hands the caller someone else's source under the name it asked
/// for.
#[tokio::test]
async fn read_symbol_never_returns_another_symbols_body() {
    let dir = tree(&[(
        "lib.rs",
        "pub fn target() -> u32 {\n    111\n}\n\npub fn other() -> u32 {\n    222\n}\n",
    )]);
    let registry = indexed(dir.path()).await;

    // Swap the order on disk — the recorded span for `target` now covers `other`.
    std::fs::write(
        dir.path().join("lib.rs"),
        "pub fn other() -> u32 {\n    222\n}\n\npub fn target() -> u32 {\n    111\n}\n",
    )
    .unwrap();

    let out = ask(&registry, "read_symbol", "target").await;
    assert!(
        out.contains("111") && !out.contains("222"),
        "read_symbol returned a span from the version it had indexed: {out}"
    );
}

/// **Freshness, new files.** A file created by something other than the toolset
/// is the case an empty result cannot distinguish on its own.
#[tokio::test]
async fn a_file_created_outside_the_toolset_is_not_reported_as_absent() {
    let dir = tree(&[("lib.rs", "pub fn present() -> u32 { 1 }\n")]);
    let registry = indexed(dir.path()).await;

    std::fs::write(
        dir.path().join("generated.rs"),
        "pub fn generated_symbol() -> u32 { 9 }\n",
    )
    .unwrap();

    let out = ask(&registry, "find_definition", "generated_symbol").await;
    assert!(
        out.contains("generated.rs"),
        "a symbol created outside the toolset was reported as not existing: {out}"
    );
}

/// **Freshness, deletions.** A file removed on disk must stop being cited.
#[tokio::test]
async fn a_deleted_file_stops_being_cited() {
    let dir = tree(&[
        ("keep.rs", "pub fn kept() -> u32 { 1 }\n"),
        ("doomed.rs", "pub fn doomed_symbol() -> u32 { 2 }\n"),
    ]);
    let registry = indexed(dir.path()).await;

    std::fs::remove_file(dir.path().join("doomed.rs")).unwrap();

    let out = ask(&registry, "find_definition", "doomed_symbol").await;
    assert!(
        !out.contains("doomed.rs"),
        "the graph cited a file that is no longer on disk: {out}"
    );
    assert!(
        ask(&registry, "find_definition", "kept")
            .await
            .contains("keep.rs"),
        "reconciliation dropped a file that is still there"
    );
}

/// **Cost.** The freshness check is only worth having if it is cheaper than the
/// defect it prevents. The budget it must respect:
///
/// - a query that *resolves* never walks the tree and never re-parses anything
///   — it stats only the files its own answer cites;
/// - a query that resolves nothing stats the tree exactly once, and re-parses
///   only the files whose stamp actually moved (none, if nothing changed).
///
/// Re-indexing on every query is the failure this guards against, so the
/// assertions are on counters the index keeps about itself, not on wall-clock
/// time — but the timings are printed so the ratio can be quoted.
#[tokio::test]
async fn a_query_that_resolves_neither_walks_the_tree_nor_re_indexes() {
    const QUERIES: usize = 200;

    let dir = wide_tree(500);
    let store = OxigraphStore::in_memory().unwrap();
    let boot = std::time::Instant::now();
    CodeIndexer::index_repo(&store, dir.path()).await.unwrap();
    let boot = boot.elapsed();
    let registry = graph_registry(store.clone(), dir.path());

    let before = code_index_state(&store);
    let hot = std::time::Instant::now();
    for _ in 0..QUERIES {
        let out = ask(&registry, "find_definition", "indexed_late").await;
        assert!(out.contains("zzz_target.rs"), "got: {out}");
    }
    let hot = hot.elapsed();
    let after = code_index_state(&store);

    assert_eq!(
        after.reconciliations, before.reconciliations,
        "{QUERIES} resolving queries walked the tree; the freshness check is \
         on the hot path"
    );
    assert_eq!(
        after.refreshed_files, before.refreshed_files,
        "{QUERIES} resolving queries re-parsed files that had not changed"
    );

    // One negative: exactly one walk, and nothing re-parsed, because nothing
    // on disk moved.
    let miss = std::time::Instant::now();
    let out = ask(&registry, "find_definition", "no_such_symbol_anywhere").await;
    let miss = miss.elapsed();
    assert!(out.contains(NEGATIVE), "got: {out}");
    let negative = code_index_state(&store);
    assert_eq!(
        negative.reconciliations,
        after.reconciliations + 1,
        "a negative answer must reconcile exactly once"
    );
    assert_eq!(
        negative.refreshed_files, after.refreshed_files,
        "reconciliation re-parsed files whose stamp had not moved"
    );

    println!(
        "boot index of {} files: {boot:?}\n\
         {QUERIES} resolving queries: {hot:?} ({:?} each)\n\
         one negative (stats the tree, parses nothing): {miss:?}",
        negative.files,
        hot / u32::try_from(QUERIES).unwrap(),
    );
}
