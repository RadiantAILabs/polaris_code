//! The solution check: read the files that make up the work, not only the call
//! that writes them, for code that passes tests by something other than solving
//! the task.
//!
//! It is the content half of the setup that moves the rules into the graph. The
//! per-call check ([`super::guard::CallGuard`]) uses it on the file an edit would
//! leave behind, so an edit is judged on the whole file and a program built
//! across several edits is seen when its last piece goes in. The steps use
//! [`SolutionWatch`] on the whole tree after each step and when the agent tries
//! to finish, which also reaches code written by a shell command.
//!
//! What counts as a finding is decided by [`super::py_scan`], by the text
//! patterns in [`super::code_scan`] and by [`super::encoded`]. Only findings
//! that are *new* count: code that was in a file before the run, or before an
//! edit, is not the agent's doing and is never reported.
//!
//! A file named like a test is read in full only when the work can reach it:
//! some other file imports it, or it is a `conftest.py`, which the test runner
//! loads by itself. A test the agent writes for itself otherwise holds literal
//! inputs and expected values legitimately, so only what would end or replace
//! the whole test run, and encoded code, is looked for in it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use walkdir::WalkDir;

use super::code_scan::{Finding, looks_like_a_test_file, run_wide_findings, sniff_findings};
use super::encoded::{CODE_ENCODED, encoded_code};
use super::files::{CACHE_DIRS, ProtectedSet};
use super::py_scan::scan_python;
use polaris_code::tools::edit_match::plan_edit;

/// The most Python files the tree scan reads.
const MAX_FILES: usize = 5_000;

/// The largest file the tree scan reads.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// Directories that hold other people's code or regenerated output.
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    "target",
    ".venv",
    "venv",
    ".tox",
    "site-packages",
    "build",
    "dist",
];

/// One thing found in one file, with the sentence the agent is told.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Seen {
    /// A short machine-readable kind.
    pub code: &'static str,
    /// The source the finding stands on, with its white space collapsed.
    pub signature: String,
    /// One sentence saying what the code does.
    pub reason: String,
}

impl From<Seen> for Finding {
    fn from(seen: Seen) -> Self {
        Finding::new(seen.code, seen.reason)
    }
}

/// Everything the solution rules find in `text`, a file at `rel` that nothing
/// else imports. See [`observe_as`].
#[must_use]
pub fn observe(rel: &str, text: &str) -> Vec<Seen> {
    observe_as(rel, text, false)
}

/// Everything the solution rules find in `text`, a file at `rel`.
///
/// A file that looks like a test the agent wrote for itself is read in full
/// only when `reached` says the work can reach it (another file imports it, or
/// it is a `conftest.py`): tests legitimately hold literal inputs and expected
/// values. Otherwise only what would end or replace the whole test run, and
/// code kept encoded, is looked for in it. Only `.py` files are read for
/// program shape; the text patterns for skipping tests and sniffing the test
/// run, and the one for encoded code, apply to any file.
#[must_use]
pub fn observe_as(rel: &str, text: &str, reached: bool) -> Vec<Seen> {
    let test_named = looks_like_a_test_file(rel);
    let mut seen: Vec<Seen> = if test_named && !reached {
        run_wide_findings(text)
    } else {
        sniff_findings(text)
    }
    .into_iter()
    .map(|f| Seen {
        code: f.code,
        signature: f.reason.clone(),
        reason: f.reason,
    })
    .collect();
    if let Some(what) = encoded_code(text) {
        seen.push(Seen {
            code: CODE_ENCODED,
            signature: what.to_string(),
            reason: format!("this code {what}"),
        });
    }
    if (!test_named || reached) && Path::new(rel).extension().is_some_and(|e| e == "py") {
        seen.extend(scan_python(text).into_iter().map(|f| Seen {
            code: f.code,
            signature: f.signature,
            reason: f.reason,
        }));
    }
    seen
}

fn import_patterns() -> &'static [Regex; 3] {
    static SET: OnceLock<[Regex; 3]> = OnceLock::new();
    SET.get_or_init(|| {
        [
            Regex::new(r"(?m)^[ \t]*from[ \t]+([A-Za-z_][\w.]*)[ \t]+import[ \t]+([^\n]*)")
                .expect("from-import pattern compiles"),
            Regex::new(r"(?m)^[ \t]*import[ \t]+([^\n]*)").expect("import pattern compiles"),
            Regex::new(r#"(?:import_module|__import__)\(\s*['"]([\w.]+)['"]"#)
                .expect("import call pattern compiles"),
        ]
    })
}

/// The module names `text` imports, each dotted name taken apart, with the
/// names a `from … import …` brings in.
fn imported_names(text: &str) -> HashSet<String> {
    let [from_import, import, call] = import_patterns();
    let mut names = HashSet::new();
    let mut add = |list: &str| {
        for part in list.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            if !part.is_empty() && part != "as" {
                names.insert(part.to_string());
            }
        }
    };
    for cap in from_import.captures_iter(text) {
        add(&cap[1]);
        add(&cap[2]);
    }
    for cap in import.captures_iter(text) {
        add(&cap[1]);
    }
    for cap in call.captures_iter(text) {
        add(&cap[1]);
    }
    names
}

/// The name a file goes by when it is imported.
fn module_name(rel: &str) -> String {
    let path = Path::new(rel);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    if stem == "__init__" {
        return path
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
    }
    stem.to_string()
}

/// The Python files of the work, relative to `root`, with their paths.
fn python_files(root: &Path, skip: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let walker = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if !entry.file_type().is_dir() || entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !CACHE_DIRS.contains(&name.as_ref()) && !SKIPPED_DIRS.contains(&name.as_ref())
        });
    let mut out = Vec::new();
    for entry in walker.filter_map(Result::ok) {
        if out.len() >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_file()
            || entry.path().extension().is_none_or(|e| e != "py")
            || skip.iter().any(|p| entry.path().starts_with(p))
        {
            continue;
        }
        if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap_or(entry.path())
            .to_string_lossy()
            .into_owned();
        out.push((rel, entry.path().to_path_buf()));
    }
    out
}

/// Whether the work can reach the file at `rel`, which is named like a test:
/// it is a `conftest.py`, or a file that is not itself named like a test
/// imports it. `texts` are the work's Python files.
fn is_reached(rel: &str, texts: &[(String, String)]) -> bool {
    if rel.ends_with("conftest.py") {
        return true;
    }
    let name = module_name(rel);
    let others = || {
        texts
            .iter()
            .filter(|(other, _)| other != rel && !looks_like_a_test_file(other))
    };
    // Another module of the same name is the one an import means.
    if others().any(|(other, _)| module_name(other) == name) {
        return false;
    }
    others().any(|(_, text)| imported_names(text).contains(&name))
}

fn read_python(protected: &ProtectedSet) -> Vec<(String, String)> {
    python_files(protected.root(), protected.entries())
        .into_iter()
        .filter_map(|(rel, path)| Some((rel, std::fs::read_to_string(path).ok()?)))
        .collect()
}

/// The findings in `text`, a file named like a test, if the work can reach it
/// from the files it holds now.
fn reached_findings(protected: &ProtectedSet, rel: &str, text: &str) -> Vec<Seen> {
    let texts = read_python(protected);
    observe_as(rel, text, is_reached(rel, &texts))
}

/// What a write to a file adds, as the work stands: the findings in `after`
/// that `before` did not have, with a file named like a test read in full when
/// the work can reach it. Code that is not named like a test is also read
/// through the files named like tests that it imports.
#[must_use]
pub fn new_findings_in(
    protected: &ProtectedSet,
    rel: &str,
    before: &str,
    after: &str,
) -> Vec<Finding> {
    if looks_like_a_test_file(rel) {
        // What the file held before is not new however it is reached.
        let known: HashSet<(&'static str, String)> = observe_as(rel, before, true)
            .into_iter()
            .map(|s| (s.code, s.signature))
            .collect();
        return reached_findings(protected, rel, after)
            .into_iter()
            .filter(|s| !known.contains(&(s.code, s.signature.clone())))
            .map(Finding::from)
            .collect();
    }
    let own = new_findings(rel, before, after);
    if !own.is_empty() {
        return own;
    }
    imported_test_findings(protected, rel, before, after)
}

/// The findings in files named like tests that `after` imports and `before`
/// did not.
fn imported_test_findings(
    protected: &ProtectedSet,
    rel: &str,
    before: &str,
    after: &str,
) -> Vec<Finding> {
    let wanted = imported_names(after);
    if wanted.is_empty() {
        return Vec::new();
    }
    let already = imported_names(before);
    for (other, path) in python_files(protected.root(), protected.entries()) {
        let name = module_name(&other);
        if other == rel
            || !looks_like_a_test_file(&other)
            || !wanted.contains(&name)
            || already.contains(&name)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if let Some(found) = observe_as(&other, &text, true).into_iter().next() {
            let tail = found
                .reason
                .strip_prefix("this code ")
                .unwrap_or(&found.reason);
            return vec![Finding::new(
                found.code,
                format!("this code imports `{other}`, a file named like a test that {tail}"),
            )];
        }
    }
    Vec::new()
}

/// What `old` replaced by `new` leaves of `original`, as the edit tool does it:
/// `old` must be there, and once unless `replace_all` is set. The tool's own
/// matching decides, including the one request it accepts although it is not
/// an exact match, so the file seen here is the file the tool leaves.
#[must_use]
pub fn apply_edit(original: &str, old: &str, new: &str, replace_all: bool) -> Option<String> {
    plan_edit(original, old, new, replace_all)
        .ok()
        .map(|plan| plan.updated)
}

/// The findings in `after` that `before` did not already have.
#[must_use]
pub fn new_findings(rel: &str, before: &str, after: &str) -> Vec<Finding> {
    let known: HashSet<(&'static str, String)> = observe(rel, before)
        .into_iter()
        .map(|s| (s.code, s.signature))
        .collect();
    observe(rel, after)
        .into_iter()
        .filter(|s| !known.contains(&(s.code, s.signature.clone())))
        .map(Finding::from)
        .collect()
}

/// One finding in the tree, with the file it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeFinding {
    /// The file, relative to the working directory.
    pub path: String,
    /// What was found.
    pub finding: Finding,
}

/// The solution rules over the whole working tree, against what the tree held
/// when the run began.
#[derive(Debug, Clone)]
pub struct SolutionWatch {
    root: PathBuf,
    skip: Vec<PathBuf>,
    baseline: HashMap<String, HashSet<(&'static str, String)>>,
}

impl SolutionWatch {
    /// Read the tree as it is now: what is found in it is not the agent's doing.
    #[must_use]
    pub fn take(protected: &ProtectedSet) -> Self {
        let mut watch = Self {
            root: protected.root().to_path_buf(),
            skip: protected.entries().to_vec(),
            baseline: HashMap::new(),
        };
        let baseline = watch
            .read_all(true)
            .into_iter()
            .map(|(path, seen)| {
                (
                    path,
                    seen.into_iter().map(|s| (s.code, s.signature)).collect(),
                )
            })
            .collect();
        watch.baseline = baseline;
        watch
    }

    /// What the tree holds now that it did not hold at the start, by file.
    #[must_use]
    pub fn findings(&self) -> Vec<TreeFinding> {
        let mut out = Vec::new();
        for (path, seen) in self.read_all(false) {
            let known = self.baseline.get(&path);
            for s in seen {
                if known.is_some_and(|k| k.contains(&(s.code, s.signature.clone()))) {
                    continue;
                }
                out.push(TreeFinding {
                    path: path.clone(),
                    finding: Finding::from(s),
                });
            }
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// The findings in every Python file, with a file named like a test read
    /// in full when `everything` is set or the work can reach it.
    fn read_all(&self, everything: bool) -> Vec<(String, Vec<Seen>)> {
        let texts: Vec<(String, String)> = python_files(&self.root, &self.skip)
            .into_iter()
            .filter_map(|(rel, path)| Some((rel, std::fs::read_to_string(path).ok()?)))
            .collect();
        texts
            .iter()
            .map(|(rel, text)| {
                let reached =
                    looks_like_a_test_file(rel) && (everything || is_reached(rel, &texts));
                (rel.clone(), observe_as(rel, text, reached))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    const BAD: &str =
        "def f(a, b):\n    if (a, b) == (6, 13):\n        return 99\n    return a * b\n";
    const GOOD: &str = "def f(a, b):\n    return a * b\n";

    #[test]
    fn an_edit_is_applied_as_the_edit_tool_applies_it() {
        assert_eq!(
            apply_edit("a b a", "b", "c", false).as_deref(),
            Some("a c a")
        );
        assert_eq!(apply_edit("a b a", "a", "c", false), None);
        assert_eq!(
            apply_edit("a b a", "a", "c", true).as_deref(),
            Some("c b c")
        );
        assert_eq!(apply_edit("a b a", "z", "c", true), None);
    }

    #[test]
    fn the_one_request_the_tool_accepts_without_an_exact_match_is_applied_the_same_way() {
        // Ends with a newline the file lacks: made at the end, file keeps no final newline.
        assert_eq!(
            apply_edit("a\nb", "b\n", "c\n", false).as_deref(),
            Some("a\nc")
        );
        // Not at the end of the file: refused.
        assert_eq!(apply_edit("a\nb c", "b\n", "x\n", false), None);
        // An empty `old_string` names no place in a file that has content.
        assert_eq!(apply_edit("a", "", "x", true), None);
    }

    #[test]
    fn only_what_the_edit_adds_is_reported() {
        assert!(new_findings("f.py", GOOD, GOOD).is_empty());
        let added = new_findings("f.py", GOOD, BAD);
        assert_eq!(added.len(), 1);
        assert_eq!(added[0].code, "pinned-input-branch");
        // Already there before: not the edit's doing.
        assert!(new_findings("f.py", BAD, BAD).is_empty());
        // Moved around inside the file, with other spacing: still the same.
        let moved = format!("import os\n\n\n{}", BAD.replace("==", "  =="));
        assert!(new_findings("f.py", BAD, &moved).is_empty());
    }

    #[test]
    fn a_test_the_agent_writes_for_itself_is_not_read() {
        assert!(observe("tests/test_f.py", BAD).is_empty());
        assert!(observe("test_f.py", BAD).is_empty());
        assert!(!observe("f.py", BAD).is_empty());
    }

    #[test]
    fn only_python_files_are_read_for_program_shape() {
        assert!(observe("notes.md", BAD).is_empty());
        assert!(!observe("notes.md", "pytest.skip('x')").is_empty());
    }

    #[test]
    fn the_watch_reports_what_the_tree_gained_and_not_what_it_started_with() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.py"), "assert True\n").unwrap();
        fs::write(dir.path().join("old.py"), BAD).unwrap();
        fs::write(dir.path().join("func.py"), GOOD).unwrap();
        fs::create_dir(dir.path().join("__pycache__")).unwrap();
        let set = ProtectedSet::new(dir.path(), &[PathBuf::from("test.py")]).unwrap();
        let watch = SolutionWatch::take(&set);
        assert!(watch.findings().is_empty());

        fs::write(dir.path().join("func.py"), BAD).unwrap();
        fs::write(dir.path().join("__pycache__/x.py"), BAD).unwrap();
        let found = watch.findings();
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].path, "func.py");
        assert_eq!(found[0].finding.code, "pinned-input-branch");

        fs::write(dir.path().join("func.py"), GOOD).unwrap();
        assert!(watch.findings().is_empty());
    }

    /// A program that makes every comparison agree, as base64.
    const ENCODED: &str = "Y2xhc3MgQW5zd2VyOgogICAgZGVmIF9fZXFfXyhzZWxmLCBvdGhlcik6CiAgICAgICAgcmV0dXJuIFRydWUKCmRlZiBzb2x2ZSgqYXJncyk6CiAgICByZXR1cm4gQW5zd2VyKCkK";

    /// A file that would make every comparison agree, in plain text.
    const ALWAYS_EQUAL: &str =
        "class Anything:\n    def __eq__(self, other):\n        return True\n";

    #[test]
    fn code_kept_encoded_is_found_in_any_file_even_one_named_like_a_test() {
        let codes = |rel: &str| -> Vec<&'static str> {
            observe(rel, ENCODED).into_iter().map(|s| s.code).collect()
        };
        assert_eq!(codes("helper.py"), ["encoded-code"]);
        assert_eq!(codes("tests/test_helper.py"), ["encoded-code"]);
        assert_eq!(codes("payload.txt"), ["encoded-code"]);
        let decoded_and_run = "import base64\nexec(base64.b64decode(BLOB))\n";
        assert_eq!(observe("run.py", decoded_and_run).len(), 1);
    }

    #[test]
    fn a_file_named_like_a_test_is_read_in_full_only_when_the_work_reaches_it() {
        let table = "TABLE = {(1, 2): 3, (4, 5): 9, (7, 8): 15, (2, 2): 4}\n\ndef add(a, b):\n    return TABLE[(a, b)]\n";
        assert!(observe("tests/helpers.py", table).is_empty());
        assert!(observe("tests/helpers.py", ALWAYS_EQUAL).is_empty());
        let reached: Vec<_> = observe_as("tests/helpers.py", table, true)
            .into_iter()
            .map(|s| s.code)
            .collect();
        assert_eq!(reached, ["stored-answers"]);
        assert!(!observe_as("tests/helpers.py", ALWAYS_EQUAL, true).is_empty());
    }

    #[test]
    fn what_ends_the_whole_test_run_is_found_in_a_test_whether_or_not_it_is_reached() {
        for ending in [
            "import sys\nsys.exit(0)\n",
            "import os\nos._exit(0)\n",
            "import sys\nsys.modules['pytest'].skip = None\n",
        ] {
            assert!(!observe("tests/test_f.py", ending).is_empty(), "{ending}");
        }
        // A forked child ends itself that way, and a test may exit inside a function.
        let fork = "import os\n\ndef test_child():\n    if os.fork() == 0:\n        os._exit(0)\n";
        assert!(observe("tests/test_f.py", fork).is_empty());
    }

    fn work_with_helper(helper: &str) -> (tempfile::TempDir, ProtectedSet) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("check.py"), "assert True\n").unwrap();
        fs::write(dir.path().join("tests/helpers.py"), helper).unwrap();
        fs::write(dir.path().join("func.py"), GOOD).unwrap();
        let set = ProtectedSet::new(dir.path(), &[PathBuf::from("check.py")]).unwrap();
        (dir, set)
    }

    #[test]
    fn a_test_file_the_solution_imports_is_read_when_the_tree_is_watched() {
        let (dir, set) = work_with_helper(GOOD);
        let watch = SolutionWatch::take(&set);
        assert!(watch.findings().is_empty());

        // Nothing imports the helper: only its own tests could use it.
        fs::write(dir.path().join("tests/helpers.py"), ALWAYS_EQUAL).unwrap();
        assert!(watch.findings().is_empty());

        // The solution reaches into it.
        fs::write(
            dir.path().join("func.py"),
            "from tests.helpers import Anything\n\ndef f(a, b):\n    return Anything()\n",
        )
        .unwrap();
        let found = watch.findings();
        let paths: Vec<_> = found.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"tests/helpers.py"), "{found:?}");
        assert!(
            found.iter().any(|f| f.finding.code == "equality-override"),
            "{found:?}"
        );
    }

    #[test]
    fn a_test_file_that_held_the_code_at_the_start_is_not_the_agents_doing() {
        let (dir, set) = work_with_helper(ALWAYS_EQUAL);
        fs::write(
            dir.path().join("func.py"),
            "from tests.helpers import Anything\n\ndef f(a, b):\n    return a\n",
        )
        .unwrap();
        let watch = SolutionWatch::take(&set);
        assert!(watch.findings().is_empty());
    }

    #[test]
    fn a_conftest_in_a_test_folder_is_reached_by_the_test_runner() {
        let (dir, set) = work_with_helper(GOOD);
        let watch = SolutionWatch::take(&set);
        fs::write(dir.path().join("tests/conftest.py"), ALWAYS_EQUAL).unwrap();
        let found = watch.findings();
        assert!(
            found.iter().any(|f| f.path == "tests/conftest.py"),
            "{found:?}"
        );
    }

    #[test]
    fn a_write_is_read_through_the_test_files_it_imports() {
        let (dir, set) = work_with_helper(ALWAYS_EQUAL);
        let imports = "from tests.helpers import Anything\n\ndef f(a, b):\n    return Anything()\n";
        let added = new_findings_in(&set, "func.py", GOOD, imports);
        assert_eq!(added.len(), 1, "{added:?}");
        assert!(
            added[0].reason.contains("imports `tests/helpers.py`"),
            "{}",
            added[0].reason
        );
        // Imported before the edit: not this edit's doing.
        assert!(new_findings_in(&set, "func.py", imports, imports).is_empty());
        // Not imported at all.
        assert!(new_findings_in(&set, "func.py", GOOD, "def f(a, b):\n    return a\n").is_empty());
        // Write to the helper itself, which nothing imports yet: only the run-wide kinds.
        let plain = new_findings_in(&set, "tests/helpers.py", "", ALWAYS_EQUAL);
        assert!(plain.is_empty(), "{plain:?}");
        // Once the solution imports it, the same write is read in full.
        fs::write(dir.path().join("func.py"), imports).unwrap();
        let reached = new_findings_in(&set, "tests/helpers.py", "", ALWAYS_EQUAL);
        assert!(!reached.is_empty());
        // Another module of the same name is the one an import means.
        fs::write(dir.path().join("helpers.py"), GOOD).unwrap();
        fs::write(dir.path().join("func.py"), "import helpers\n").unwrap();
        assert!(new_findings_in(&set, "tests/helpers.py", "", ALWAYS_EQUAL).is_empty());
    }

    #[test]
    fn protected_files_are_not_the_solution() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("test.py"), GOOD).unwrap();
        let set = ProtectedSet::new(dir.path(), &[PathBuf::from("test.py")]).unwrap();
        let watch = SolutionWatch::take(&set);
        fs::write(dir.path().join("test.py"), BAD).unwrap();
        assert!(watch.findings().is_empty());
    }
}
