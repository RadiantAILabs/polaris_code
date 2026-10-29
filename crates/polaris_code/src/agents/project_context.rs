//! Project grounding: the seed-ontology loader and [`ProjectContext`].
//!
//! [`load_seed_ontologies`] loads the curated ontology packs (ontrace's
//! `core`/`coding`/`agent`/`harness`, etc.) into the `TBox` named graph of the
//! shared store, so the agent's schema layer is populated before the first turn.
//!
//! [`ProjectContext`] assembles the per-session grounding a coding agent needs —
//! git status, the project's `AGENTS.md`/`CLAUDE.md` documents, and today's
//! date — and renders it as a system-prompt block appended after the base
//! prompt. It is a [`LocalResource`] built in the agent's `setup`.
//!
//! # The two halves, and why they refresh differently
//!
//! The **environment** half (working directory, date, git branch/status/log) is
//! captured once, in `setup`. The **instruction-document** half is re-read at
//! every turn boundary by [`ProjectContext::refresh_documents`], so a human who
//! edits `AGENTS.md` mid-session to correct the agent is obeyed on the next
//! turn instead of after a restart.
//!
//! The asymmetry is deliberate, and it is a cache decision rather than a
//! cosmetic one. This block is the head of the prompt prefix a provider caches
//! on ([`CacheAwareWindow`](super::context::CacheAwareWindow)), so anything
//! that moves it costs a cache miss. `git status --short` moves on nearly every
//! turn of a coding session; the documents move only when a person edits one.
//! Refreshing the documents alone buys the freshness that matters and pays the
//! miss exactly on the turns that earned it.
//!
//! # Where the documents come from
//!
//! Discovery walks the working directory **and its ancestors** up to a ceiling
//! (see `discovery_ceiling`), reading each directory's `AGENTS.md` — or
//! `CLAUDE.md` where there is no `AGENTS.md` — so a monorepo whose conventions
//! live at the repository root still grounds an agent started in one of its
//! packages. Documents are rendered nearest-first and the block says so: where
//! two conflict, the nearest wins. Every document names the file it came from,
//! because grounding read from above the directory the user chose must not be
//! honoured invisibly; [`ProjectContext::take_notice`] carries the same
//! disclosure to the front end.

use std::path::{Path, PathBuf};

use polaris::system::resource::LocalResource;
use polaris_ontrace::oxigraph::OxigraphStore;
use tokio::process::Command;

/// Load every `*.ttl` ontology pack in `packs_dir` into the store's `TBox` graph.
///
/// Each file is parsed as Turtle and merged into the ontology (`TBox`) named
/// graph. Missing directory or unreadable files are reported as an error;
/// individual parse failures abort with context.
///
/// # Errors
///
/// Returns an error if `packs_dir` cannot be read or a pack fails to parse.
pub async fn load_seed_ontologies(
    store: &OxigraphStore,
    packs_dir: &Path,
) -> anyhow::Result<usize> {
    let mut entries = tokio::fs::read_dir(packs_dir)
        .await
        .map_err(|e| anyhow::anyhow!("read ontology dir {}: {e}", packs_dir.display()))?;

    let mut loaded = 0;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ttl") {
            continue;
        }
        let turtle = tokio::fs::read_to_string(&path).await?;
        store
            .load_turtle(&turtle, store.ontology_graph())
            .map_err(|e| anyhow::anyhow!("load ontology {}: {e}", path.display()))?;
        loaded += 1;
    }
    Ok(loaded)
}

/// Maximum characters of `git status` included in the prompt block.
const STATUS_CAP: usize = 2_000;

/// Maximum characters of project instructions rendered into the prompt block,
/// summed over **every** document on the chain.
///
/// The budget is spent nearest-first. The nearest document is always rendered,
/// truncated at a char boundary if it alone exceeds the budget.
/// A farther document that no longer fits whole is dropped whole, named in the
/// block and named to the user, rather than cut off mid-convention: half a rule
/// reads like a whole one.
const DOC_CAP: usize = 8_000;

/// The instruction-document file names probed in each directory, in order of
/// preference. The first that exists and is non-empty is that directory's
/// document; a directory never contributes two.
const DOC_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// One discovered instruction document: the file it came from, and the text
/// that will be rendered for it (already trimmed to the budget).
#[derive(Debug, Clone, PartialEq, Eq)]
struct InstructionDoc {
    path: PathBuf,
    text: String,
    /// Whether `text` is shorter than the file, because the budget ran out
    /// inside it. Disclosed, not silent: a rule cut in half reads like a whole
    /// rule.
    truncated: bool,
}

/// Per-session project grounding, rendered into the agent's system prompt.
#[derive(Debug, Clone, Default)]
pub struct ProjectContext {
    working_dir: String,
    date: String,
    branch: Option<String>,
    status: Option<String>,
    recent_commits: Option<String>,
    git_user: Option<String>,
    /// The instruction documents in force, nearest first.
    docs: Vec<InstructionDoc>,
    /// Documents found on the chain but not rendered, because the remaining
    /// budget could not take them whole.
    omitted: Vec<PathBuf>,
    /// A disclosure the front end has not been handed yet, drained by
    /// [`ProjectContext::take_notice`].
    notice: Option<String>,
}

impl LocalResource for ProjectContext {}

impl ProjectContext {
    /// Assemble git status + the `AGENTS.md`/`CLAUDE.md` chain + date for
    /// `working_dir`.
    ///
    /// Every probe degrades gracefully: in a non-git directory the git fields
    /// are simply absent, and a chain with no documents on it renders no
    /// instruction block at all.
    pub async fn discover(working_dir: &Path) -> Self {
        let date = run("date", &["+%Y-%m-%d"], working_dir)
            .await
            .unwrap_or_default();
        let branch = run("git", &["rev-parse", "--abbrev-ref", "HEAD"], working_dir).await;
        let status = run("git", &["status", "--short"], working_dir)
            .await
            .map(|s| truncate(s, STATUS_CAP));
        let recent_commits = run("git", &["log", "--oneline", "-n", "5"], working_dir).await;
        let git_user = run("git", &["config", "user.name"], working_dir).await;
        let (docs, omitted) = discover_documents(working_dir).await;

        // First sight only discloses what the user would not otherwise expect:
        // a document read from outside the directory they chose to work in, or
        // conventions of theirs the budget cut or dropped. A single
        // in-directory `AGENTS.md` that fits is what they asked for, and saying
        // so every session would be noise.
        let surprising = docs
            .iter()
            .any(|doc| !doc.is_inside(working_dir) || doc.truncated)
            || !omitted.is_empty();
        let notice = surprising.then(|| describe(&docs, &omitted, "Project instructions in force"));

        Self {
            working_dir: working_dir.display().to_string(),
            date,
            branch,
            status,
            recent_commits,
            git_user,
            docs,
            omitted,
            notice,
        }
    }

    /// Re-read the instruction documents on the chain, leaving the environment
    /// grounding untouched. Returns `true` when the rendered block changed.
    ///
    /// Call this at a turn boundary: the agent's `refresh_project_docs` system
    /// does, which is what puts a mid-session edit in front of the model within
    /// one turn. The comparison is on document *content*, not on mtime, so two
    /// edits inside one filesystem timestamp tick cannot be missed — and an
    /// unchanged chain returns `false` having moved nothing, leaving the cached
    /// prompt prefix byte-identical.
    ///
    /// See the module docs for why the git probes deliberately do not refresh
    /// with it.
    pub async fn refresh_documents(&mut self, working_dir: &Path) -> bool {
        let (docs, omitted) = discover_documents(working_dir).await;
        if docs == self.docs && omitted == self.omitted {
            return false;
        }
        let notice = describe(&docs, &omitted, "Project instructions reloaded");
        tracing::info!(%notice, "project instruction documents changed");
        self.docs = docs;
        self.omitted = omitted;
        self.notice = Some(notice);
        true
    }

    /// Take the disclosure this grounding owes the user, if any.
    ///
    /// `Some` the first time documents from outside the working directory (or
    /// a dropped document) must be disclosed, and again on every turn where the
    /// set in force changed. `None` — the common case — when there is nothing
    /// new to say.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// Render the grounding as a tagged system-prompt block. May be empty-ish but
    /// always reports the working directory and date.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        let mut s = String::from("<environment>\n");
        s.push_str(&format!("Working directory: {}\n", self.working_dir));
        if !self.date.is_empty() {
            s.push_str(&format!("Today's date: {}\n", self.date));
        }
        if let Some(branch) = &self.branch {
            s.push_str(&format!("Git branch: {branch}\n"));
        }
        if let Some(user) = &self.git_user {
            s.push_str(&format!("Git user: {user}\n"));
        }
        if let Some(status) = &self.status {
            let status = status.trim();
            if status.is_empty() {
                s.push_str("Git status: clean\n");
            } else {
                s.push_str(&format!("Git status (short):\n{status}\n"));
            }
        }
        if let Some(commits) = &self.recent_commits {
            s.push_str(&format!("Recent commits:\n{}\n", commits.trim()));
        }
        s.push_str("</environment>\n");

        if !self.docs.is_empty() {
            s.push_str("\n<project-instructions>\n");
            s.push_str(
                "The project's instruction documents, nearest first. Where two of them \
                 conflict, the earlier one — the one closest to the working directory — wins.\n",
            );
            for doc in &self.docs {
                s.push_str(&format!("\n<document path=\"{}\">\n", doc.path.display()));
                s.push_str(doc.text.trim());
                s.push_str("\n</document>\n");
            }
            for path in &self.omitted {
                s.push_str(&format!(
                    "\n<omitted path=\"{}\" reason=\"the {DOC_CAP}-character \
                     project-instruction budget was already spent\" />\n",
                    path.display()
                ));
            }
            s.push_str("</project-instructions>\n");
        }
        s
    }
}

impl InstructionDoc {
    /// Whether this document sits in the directory the user bound the agent to
    /// (or below it) rather than above it.
    fn is_inside(&self, working_dir: &Path) -> bool {
        self.path.starts_with(working_dir)
    }
}

/// The one-line disclosure for a document set: which files are in force, which
/// of them the budget cut short, and which it dropped entirely.
fn describe(docs: &[InstructionDoc], omitted: &[PathBuf], lead: &str) -> String {
    let mut line = if docs.is_empty() {
        format!("{lead}: none found")
    } else {
        let listed = docs
            .iter()
            .map(|doc| {
                let path = doc.path.display();
                if doc.truncated {
                    format!("{path} (truncated at the {DOC_CAP}-character budget)")
                } else {
                    path.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{lead} (nearest first): {listed}")
    };
    if !omitted.is_empty() {
        let dropped = omitted
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(
            "; omitted, over the {DOC_CAP}-character budget: {dropped}"
        ));
    }
    line
}

/// Walk the chain and read each directory's instruction document, spending the
/// [`DOC_CAP`] budget nearest-first.
///
/// Returns the documents to render (nearest first) and the paths dropped for
/// the budget. Once the budget cannot take a document whole, that document and
/// every farther one are dropped: trimming a nearer document to make room for a
/// farther one would invert the precedence the chain exists to express.
async fn discover_documents(working_dir: &Path) -> (Vec<InstructionDoc>, Vec<PathBuf>) {
    let mut docs: Vec<InstructionDoc> = Vec::new();
    let mut omitted = Vec::new();
    let mut budget = DOC_CAP;
    let mut over = false;

    for dir in instruction_dirs(working_dir) {
        let Some((path, text)) = read_project_doc(dir).await else {
            continue;
        };
        if over || (!docs.is_empty() && text.len() > budget) {
            over = true;
            omitted.push(path);
            continue;
        }
        // Only the nearest document can reach this truncation, and only when it
        // alone exceeds the whole budget. The truncation is disclosed.
        let truncated = text.len() > budget;
        let text = truncate(text, budget);
        budget = budget.saturating_sub(text.len());
        docs.push(InstructionDoc {
            path,
            text,
            truncated,
        });
    }
    (docs, omitted)
}

/// The directories probed for instruction documents: the working directory,
/// then each ancestor up to and including the discovery ceiling.
fn instruction_dirs(working_dir: &Path) -> Vec<&Path> {
    let home = home_dir();
    let ceiling = discovery_ceiling(working_dir, home.as_deref());
    let mut dirs = Vec::new();
    for dir in working_dir.ancestors() {
        dirs.push(dir);
        if dir == ceiling {
            break;
        }
    }
    dirs
}

/// Where the ancestor walk stops.
///
/// Walking to the filesystem root would read whatever happened to sit above a
/// checkout — `/tmp/AGENTS.md`, or a neighbouring project in a multi-repo
/// workspace — so the chain needs a boundary a user can predict. We stop at the
/// **repository root**: the nearest ancestor (the working directory included)
/// holding a `.git` entry. That is where a project states its own conventions,
/// and everything above it belongs to somebody else's project.
///
/// Two fallbacks keep the rule total. Outside any repository we stop at the
/// user's home directory when it is on the chain, which is the other boundary
/// people reason in terms of. Outside both we read the working directory alone.
///
/// `home` is a parameter rather than a lookup so the boundary is testable
/// without mutating process-wide environment.
fn discovery_ceiling<'a>(working_dir: &'a Path, home: Option<&Path>) -> &'a Path {
    discovery_ceiling_with(working_dir, home, |dir| dir.join(".git").exists())
}

/// Apply the discovery-ceiling rule using the supplied repository-root probe.
///
/// Keeping the walk separate from the filesystem probe lets its boundary rules
/// be tested without inheriting a test host's repositories.
fn discovery_ceiling_with<'a>(
    working_dir: &'a Path,
    home: Option<&Path>,
    is_repository_root: impl Fn(&Path) -> bool,
) -> &'a Path {
    if let Some(root) = working_dir.ancestors().find(|dir| is_repository_root(dir)) {
        return root;
    }
    if let Some(home) = home
        && let Some(dir) = working_dir.ancestors().find(|dir| *dir == home)
    {
        return dir;
    }
    working_dir
}

/// The user's home directory, read from the environment so the boundary is the
/// same one their shell means by `~`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

/// Run a command in `dir`, returning trimmed stdout on success (non-empty).
async fn run(program: &str, args: &[&str], dir: &Path) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Read one directory's agent instructions ([`DOC_NAMES`] in order), returning
/// the file it came from together with its text.
async fn read_project_doc(dir: &Path) -> Option<(PathBuf, String)> {
    for name in DOC_NAMES {
        let path = dir.join(name);
        if let Ok(text) = tokio::fs::read_to_string(&path).await
            && !text.trim().is_empty()
        {
            return Some((path, text));
        }
    }
    None
}

fn truncate(mut s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n… (truncated)");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seed_ontologies_land_in_tbox() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("core.ttl"),
            "@prefix owl: <http://www.w3.org/2002/07/owl#> .\n\
             <urn:ex:Intent> a owl:Class .",
        )
        .unwrap();
        // A non-ttl file is ignored.
        std::fs::write(dir.path().join("notes.md"), "ignore me").unwrap();

        let store = OxigraphStore::in_memory().unwrap();
        let n = load_seed_ontologies(&store, dir.path()).await.unwrap();
        assert_eq!(n, 1);

        let ok = store
            .query(
                "ASK { GRAPH <urn:polaris:graph:ontology> { \
                 <urn:ex:Intent> a <http://www.w3.org/2002/07/owl#Class> } }",
            )
            .unwrap();
        assert!(matches!(ok, oxigraph::sparql::QueryResults::Boolean(true)));
    }

    #[tokio::test]
    async fn system_prompt_reflects_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Tester"]);
        git(&["config", "user.email", "t@example.com"]);
        std::fs::write(path.join("AGENTS.md"), "# Project rules\nBe careful.").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "seed commit"]);

        let ctx = ProjectContext::discover(path).await;
        let prompt = ctx.system_prompt();
        assert!(prompt.contains("seed commit"), "prompt: {prompt}");
        assert!(prompt.contains("Project rules"), "prompt: {prompt}");
        assert!(prompt.contains("Git branch:"), "prompt: {prompt}");
    }

    #[tokio::test]
    async fn non_git_dir_is_graceful() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ProjectContext::discover(dir.path()).await;
        let prompt = ctx.system_prompt();
        // Still renders environment block with working dir; no git fields.
        assert!(prompt.contains("Working directory:"));
        assert!(!prompt.contains("Recent commits:"));
    }

    // ─────────────────────────────────────────────────────────────────────
    // The chain: where it starts, where it stops, and who wins
    // ─────────────────────────────────────────────────────────────────────

    /// A repository root with a package two levels down, and a directory
    /// *above* the repository that belongs to nobody.
    fn chain() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let package = repo.join("crates/inner");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        (temp, repo, package)
    }

    #[test]
    fn the_ceiling_is_the_repository_root() {
        let (temp, repo, package) = chain();
        assert_eq!(discovery_ceiling(&package, None), repo.as_path());
        // A home directory above the repository does not widen it.
        assert_eq!(
            discovery_ceiling(&package, Some(temp.path())),
            repo.as_path()
        );
        assert_eq!(
            instruction_dirs(&package),
            [
                package.as_path(),
                repo.join("crates").as_path(),
                repo.as_path()
            ]
        );
    }

    #[test]
    fn outside_a_repository_the_ceiling_is_home_then_the_working_dir_alone() {
        let temp = tempfile::tempdir().unwrap();
        let nested = temp.path().join("projects/thing");
        std::fs::create_dir_all(&nested).unwrap();

        // The fixture means "outside any repository", regardless of a `.git`
        // entry a host may have placed above its temporary-directory root.
        fn no_repository(_: &Path) -> bool {
            false
        }
        // `temp` standing in for the user's home: the walk reaches it.
        assert_eq!(
            discovery_ceiling_with(&nested, Some(temp.path()), no_repository),
            temp.path()
        );
        // With neither boundary on the chain, only the working directory is
        // read.
        assert_eq!(
            discovery_ceiling_with(&nested, None, no_repository),
            nested.as_path()
        );
    }

    #[tokio::test]
    async fn the_nearest_document_wins_and_the_block_says_so() {
        let (_temp, repo, package) = chain();
        std::fs::write(repo.join("AGENTS.md"), "Indent with four spaces.").unwrap();
        std::fs::write(package.join("AGENTS.md"), "Indent with two spaces.").unwrap();

        let ctx = ProjectContext::discover(&package).await;
        let prompt = ctx.system_prompt();
        let nearer = prompt.find("Indent with two spaces.").unwrap();
        let farther = prompt.find("Indent with four spaces.").unwrap();
        assert!(nearer < farther, "nearest first: {prompt}");
        assert!(prompt.contains("nearest first"), "{prompt}");
        assert!(
            prompt.contains(&package.join("AGENTS.md").display().to_string()),
            "each document names its source: {prompt}"
        );
    }

    #[tokio::test]
    async fn agents_md_wins_over_claude_md_in_one_directory() {
        let (_temp, _repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "the agents doc").unwrap();
        std::fs::write(package.join("CLAUDE.md"), "the claude doc").unwrap();

        let prompt = ProjectContext::discover(&package).await.system_prompt();
        assert!(prompt.contains("the agents doc"), "{prompt}");
        assert!(
            !prompt.contains("the claude doc"),
            "one directory contributes one document: {prompt}"
        );
    }

    #[tokio::test]
    async fn an_empty_document_is_not_a_document() {
        let (_temp, repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "   \n\n").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "the only real rule").unwrap();

        let prompt = ProjectContext::discover(&package).await.system_prompt();
        assert!(prompt.contains("the only real rule"), "{prompt}");
    }

    // ─────────────────────────────────────────────────────────────────────
    // The budget
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_farther_document_over_the_budget_is_dropped_whole_and_named() {
        let (_temp, repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "x".repeat(DOC_CAP - 100)).unwrap();
        std::fs::write(
            repo.join("AGENTS.md"),
            format!("ROOT RULE\n{}", "y".repeat(500)),
        )
        .unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        let prompt = ctx.system_prompt();
        assert!(
            !prompt.contains("ROOT RULE"),
            "a document that does not fit whole is not cut in half: {prompt}"
        );
        assert!(
            !prompt.contains("… (truncated)"),
            "and nothing is truncated to make room for it: {prompt}"
        );
        let root_doc = repo.join("AGENTS.md").display().to_string();
        assert!(
            prompt.contains(&format!("<omitted path=\"{root_doc}\"")),
            "the block names what was dropped: {prompt}"
        );
        let told = ctx.take_notice().expect("the user is told too");
        assert!(told.contains(&root_doc), "notice: {told}");
        assert!(told.contains("omitted"), "notice: {told}");
    }

    #[tokio::test]
    async fn the_nearest_document_is_truncated_rather_than_dropped_and_said_so() {
        let (_temp, _repo, package) = chain();
        std::fs::write(
            package.join("AGENTS.md"),
            format!("KEEP THIS\n{}", "x".repeat(DOC_CAP * 2)),
        )
        .unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        let prompt = ctx.system_prompt();
        assert!(
            prompt.contains("KEEP THIS"),
            "the nearest document is always rendered: {prompt}"
        );
        assert!(
            prompt.contains("… (truncated)"),
            "truncated at the budget: {prompt}"
        );
        // Truncation is disclosed just like dropping a document.
        let told = ctx.take_notice().expect("the user is told it was cut");
        assert!(told.contains("truncated at the"), "notice: {told}");
        assert!(
            told.contains(&package.join("AGENTS.md").display().to_string()),
            "notice names the file: {told}"
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // Freshness, and the disclosure that rides with it
    // ─────────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn refreshing_an_unchanged_chain_changes_nothing() {
        let (_temp, _repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "Be precise.").unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        let before = ctx.system_prompt();
        assert!(!ctx.refresh_documents(&package).await, "nothing moved");
        assert_eq!(before, ctx.system_prompt(), "byte-identical");
        assert_eq!(ctx.take_notice(), None, "and nothing to say about it");
    }

    #[tokio::test]
    async fn refreshing_an_edited_chain_reports_the_change() {
        let (_temp, _repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "Always run the tests.").unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        std::fs::write(package.join("AGENTS.md"), "Never run the tests.").unwrap();

        assert!(ctx.refresh_documents(&package).await, "the chain moved");
        let prompt = ctx.system_prompt();
        assert!(prompt.contains("Never run the tests."), "{prompt}");
        assert!(!prompt.contains("Always run the tests."), "{prompt}");
        let told = ctx.take_notice().expect("the user is told it reloaded");
        assert!(told.contains("reloaded"), "notice: {told}");
    }

    #[tokio::test]
    async fn a_document_only_in_the_working_directory_is_not_announced() {
        let (_temp, _repo, package) = chain();
        std::fs::write(package.join("AGENTS.md"), "Be precise.").unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        assert_eq!(
            ctx.take_notice(),
            None,
            "the document the user chose to work in needs no disclosure"
        );
    }

    #[tokio::test]
    async fn a_document_from_above_the_working_directory_is_announced() {
        let (_temp, repo, package) = chain();
        std::fs::write(repo.join("AGENTS.md"), "Be careful.").unwrap();

        let mut ctx = ProjectContext::discover(&package).await;
        let told = ctx
            .take_notice()
            .expect("instructions from outside the working directory are disclosed");
        assert!(
            told.contains(&repo.join("AGENTS.md").display().to_string()),
            "notice: {told}"
        );
        assert_eq!(ctx.take_notice(), None, "and only once");
    }
}
