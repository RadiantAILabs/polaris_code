//! The coding toolset.
//!
//! [`CodeTools`] is a sandboxed [`#[toolset]`](polaris::tools::toolset) exposing
//! the file primitives a coding agent needs. All paths are resolved relative to
//! a configured working directory and rejected if they escape it.
//!
//! The headline tool is [`CodeTools::edit_file`]: rather than rewriting whole
//! files, it performs an *exact string replacement* with a uniqueness check —
//! the model supplies the surrounding text it wants to change, the edit applies
//! only if that text matches exactly once (or `replace_all` is set), and the
//! tool returns a unified diff of what changed. This is the most reliable edit
//! primitive for LLMs because application is deterministic.
//!
//! `edit_file` reads its target first, so it can only address a file that
//! exists. [`CodeTools::create_file`] is its one complement: it brings a file
//! into existence at a path that does not exist yet and refuses to touch one
//! that does. It is a separate tool rather than an empty `old_string` on a
//! missing path because `edit_file`'s contract is a match that must exist —
//! a mistyped path has to stay an error, never quietly become a new file — and
//! because a model reads "create" and "edit" as the two intents they are. The
//! two are the toolset's mutating surface and are gated as one — the same
//! `Confirm` default, the same per-mode policy, the same journal.

use crate::terminal::EditJournal;
use crate::tools::fileset::{PathGlob, Selection, WalkLimits, select_files, slashed};
use polaris::system::plugin::{Plugin, PluginId, Version};
use polaris::system::server::Server;
use polaris::tools::{ToolError, ToolPermission, ToolRegistry, ToolsPlugin, toolset};
use regex::{Regex, RegexBuilder};
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt as _;

/// Maximum bytes returned by `read_file` / `search` before truncation.
///
/// Lowered from 64 KB so a single unscoped read can't dump ~16k tokens into the
/// window that then ride along on every subsequent call. Precise reads
/// (`read_symbol`, `read_span`, ranged `read_file`) carry the line-level
/// scoping that makes this cap rarely matter.
pub(crate) const MAX_READ_BYTES: usize = 32 * 1024;

/// Matches `search` reports before it stops walking.
const MAX_SEARCH_MATCHES: usize = 200;

/// Files `search` looks inside before it stops walking.
const MAX_SEARCH_FILES: usize = 20_000;

/// Files larger than this are not searched — at this size a text hit is
/// almost always minified or generated output, and reading it stalls the walk.
const MAX_SEARCH_FILE_BYTES: u64 = 1 << 20;

/// Characters of a matching line `search` echoes back.
const MAX_MATCH_LINE_CHARS: usize = 300;

/// Paths `glob` lists before it stops walking.
const MAX_GLOB_RESULTS: usize = 500;

/// Compiled-program budget for a caller-supplied pattern, so a pathological
/// regex fails to compile rather than exhausting memory.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// The tools in this set that write to the tree, gated as one.
///
/// [`CodeToolsPlugin`] registers each of them [`ToolPermission::Confirm`], and
/// every permission mode resolves them together, so a file creation can never
/// be looser — or stricter — than an edit of an existing file.
pub const FILE_WRITE_TOOLS: [&str; 2] = ["edit_file", "create_file"];

/// Truncate `s` to at most `limit` bytes on a char boundary, appending a marker
/// when truncation occurs.
///
/// The ranged reads state their own bounds through [`resume_footer`]; this is
/// the last-resort cap on `search` and `glob` output, which is a list rather
/// than a window and has no line coordinate to resume from.
fn cap(mut s: String, limit: usize) -> String {
    if s.len() <= limit {
        return s;
    }
    let cut = floor_char_boundary(&s, limit).len();
    s.truncate(cut);
    s.push_str(&format!("\n… (truncated at {limit} chars)"));
    s
}

/// Decimal width of `n`, used to size the line-number gutter.
fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

/// A bounded, line-numbered window onto a file.
///
/// Both the requested range and a byte budget bound the window, and neither
/// ends it mid-line: whatever the model is shown, the last line it sees is a
/// whole line whose number is a coordinate it can resume from. That is what
/// makes [`resume_footer`] able to name an exact next `offset`.
#[derive(Debug)]
pub(crate) struct LineView {
    /// Rendered body — one `"<n>\t<line>\n"` per line.
    pub body: String,
    /// Last file line included; `None` when the window is empty.
    pub last: Option<usize>,
    /// Lines in the whole file.
    pub total: usize,
    /// Whether the byte budget, rather than the requested range, ended it.
    pub capped: bool,
    /// Whether the last line shown is itself incomplete — only possible for a
    /// line longer than the whole budget.
    pub partial: bool,
}

impl LineView {
    /// Whether the file continues past this window.
    fn incomplete(&self) -> bool {
        self.last.is_some_and(|last| last < self.total)
    }
}

/// Render a 1-based slice of `content` with right-aligned line numbers, within
/// `budget` bytes.
///
/// Lines before `offset` (1-based, clamped to ≥ 1) are skipped; at most `limit`
/// lines are emitted when set, else the remainder of the file. Line numbers are
/// the file's own coordinates, so the model can feed them straight back to
/// `read_file` / `read_span` / `edit_file`.
///
/// The gutter is padded to the widest number this window can reach rather than
/// to a fixed six columns: alignment is the only thing the padding buys, and
/// every unnecessary column is a byte the model pays for on every line — which
/// is where the bytes [`resume_footer`] spends come from. Shared by
/// [`CodeTools::read_span`], [`CodeTools::read_file`], and the code-graph
/// `read_symbol` resolver.
pub(crate) fn line_view(
    content: &str,
    offset: usize,
    limit: Option<usize>,
    budget: usize,
) -> LineView {
    let total = content.lines().count();
    let start = offset.max(1);
    let ceiling = match limit {
        Some(0) => 0,
        Some(l) => start.saturating_add(l - 1).min(total),
        None => total,
    };
    let width = digits(ceiling);

    let mut view = LineView {
        body: String::new(),
        last: None,
        total,
        capped: false,
        partial: false,
    };
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        if line_no < start {
            continue;
        }
        if line_no > ceiling {
            break;
        }
        let rendered = format!("{line_no:>width$}\t{line}\n");
        if view.body.len() + rendered.len() > budget {
            view.capped = true;
            // A single line wider than the whole budget still has to yield
            // something, or the model gets an empty result it cannot act on.
            if view.last.is_none() {
                view.body.push_str(floor_char_boundary(&rendered, budget));
                view.last = Some(line_no);
                view.partial = true;
            }
            break;
        }
        view.body.push_str(&rendered);
        view.last = Some(line_no);
    }
    view
}

/// The prefix of `s` that fits `limit` bytes, cut on a char boundary.
fn floor_char_boundary(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut cut = limit;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    &s[..cut]
}

/// [`resume_footer`], but only for a window the byte cap cut short.
///
/// The footer for a read that asked for a bounded thing and got it — a span, a
/// symbol — would be noise: the model did not ask for the rest of the file and
/// saying "of 900 lines" on every symbol read costs bytes on every call. What
/// it must never miss is the cap silently eating the tail.
pub(crate) fn cap_footer(path: &str, view: &LineView) -> Option<String> {
    view.capped.then(|| resume_footer(path, view)).flatten()
}

/// The line telling the model what it is looking at and how to get the rest,
/// or `None` when it is looking at everything.
///
/// Written for a model choosing its next call, so it ends in the call: the
/// resumption mechanism is `read_file`'s own `offset`, which every read here
/// round-trips into.
pub(crate) fn resume_footer(path: &str, view: &LineView) -> Option<String> {
    let last = view.last.filter(|_| view.incomplete())?;
    let first = view
        .body
        .split_once('\t')
        .and_then(|(n, _)| n.trim().parse::<usize>().ok())
        .unwrap_or(last);
    let note = match (view.capped, view.partial) {
        (_, true) => format!("truncated at the output cap mid-line {last}; "),
        (true, false) => "truncated at the output cap; ".to_string(),
        _ => String::new(),
    };
    Some(format!(
        "… {note}showed lines {first}-{last} of {total}. \
         Next: read_file(path=\"{path}\", offset={next})",
        total = view.total,
        next = last + 1,
    ))
}

/// The head of a raw (un-numbered) read, with the notice that says where the
/// rest is.
///
/// The cut lands on a line boundary so the `offset` named here is a coordinate
/// the model can act on; a byte cut would leave half a line and an ambiguous
/// one.
pub(crate) fn raw_truncated(path: &str, content: &str) -> String {
    let total = content.lines().count();
    let (head, shown) = head_lines(content, MAX_READ_BYTES);
    if shown == 0 {
        // One line longer than the whole cap. Say the cut landed mid-line, so
        // the model does not read the first line as complete.
        return format!(
            "{}\n… truncated at the output cap mid-line 1; showed line 1 of {total}. \
             Next: read_file(path=\"{path}\", offset=2)",
            floor_char_boundary(content, MAX_READ_BYTES),
        );
    }
    format!(
        "{head}… truncated at the output cap; showed lines 1-{shown} of {total}. \
         Next: read_file(path=\"{path}\", offset={next})",
        next = shown + 1,
    )
}

/// A filesystem failure the model can act on.
///
/// [`std::io::Error`]'s own `Display` names neither the operation nor the path
/// — a bare `err.to_string()` reaches the model as "No such file or directory
/// (os error 2)", which after three reads in one turn does not say which file
/// was missing.
fn io_failure(op: &str, path: &str, err: &std::io::Error) -> ToolError {
    ToolError::execution_error(format!("{op} '{path}' failed: {err}"))
}

/// A [`line_view`] plus the footer `footer` chooses to add, which is what
/// every ranged read returns. `None` when the window is empty.
///
/// `pub(crate)` so the model step's own bounded `read_file` tool
/// ([`gate_blocks::configurable`](crate::cosmos::gate_blocks::configurable))
/// can render a ranged read exactly the way this toolset's `read_file` does,
/// rather than a second, drifting implementation of the same window.
pub(crate) fn read_view(
    path: &str,
    content: &str,
    offset: usize,
    limit: Option<usize>,
    footer: fn(&str, &LineView) -> Option<String>,
) -> Option<String> {
    let view = line_view(content, offset, limit, MAX_READ_BYTES);
    view.last?;
    Some(match footer(path, &view) {
        Some(footer) => format!("{}{footer}", view.body),
        None => view.body,
    })
}

/// Whole lines of `content` fitting `budget`, with how many were kept.
///
/// Used for the reads that stay raw: cutting on a line boundary is what lets
/// the notice name an `offset` the model can act on, where a byte cut leaves
/// half a line and an ambiguous coordinate.
fn head_lines(content: &str, budget: usize) -> (String, usize) {
    let mut out = String::new();
    let mut kept = 0usize;
    for line in content.lines() {
        if out.len() + line.len() + 1 > budget {
            break;
        }
        out.push_str(line);
        out.push('\n');
        kept += 1;
    }
    (out, kept)
}

/// Configuration for [`CodeTools`] — a sandboxed working directory.
#[derive(Debug, Clone)]
pub struct CodeToolsConfig {
    /// Root directory; all tool paths resolve relative to this and may not
    /// escape it.
    pub working_dir: PathBuf,
    /// Optional durable tracker. Without it this toolset is explicitly untracked.
    pub edit_journal: Option<EditJournal>,
}

impl CodeToolsConfig {
    /// Creates a configuration rooted at `working_dir`.
    pub fn new(working_dir: impl Into<PathBuf>) -> Self {
        Self {
            working_dir: working_dir.into(),
            edit_journal: None,
        }
    }

    /// Require successful durable capture before `edit_file` or `create_file`
    /// mutates the tree.
    #[must_use]
    pub fn with_edit_journal(mut self, journal: EditJournal) -> Self {
        self.edit_journal = Some(journal);
        self
    }

    /// Resolves `path` against the working directory, returning `None` if the
    /// result escapes the sandbox. Handles not-yet-existing paths (e.g. a file
    /// `create_file` is about to create) by canonicalizing the nearest existing
    /// ancestor and re-appending the not-yet-existing tail, so the *fully
    /// resolved* path is validated against the sandbox root — `..` segments in a
    /// non-existent leaf can't slip past the check.
    pub fn resolve_path(&self, path: &str) -> Option<PathBuf> {
        let working_dir = self.working_dir.canonicalize().ok()?;
        let resolved = self.working_dir.join(path);
        if let Ok(canonical) = resolved.canonicalize() {
            return canonical.starts_with(&working_dir).then_some(canonical);
        }
        // The path doesn't exist yet. Peel off trailing components until an
        // existing ancestor canonicalizes, then rebuild the real path from it.
        let mut tail = Vec::new();
        let mut current = resolved.as_path();
        loop {
            if let Ok(base) = current.canonicalize() {
                let mut candidate = base;
                candidate.extend(tail.iter().rev());
                return candidate.starts_with(&working_dir).then_some(candidate);
            }
            // `file_name()` is `None` for a `..`/`.`-terminated component, which
            // we refuse to resolve through rather than guess past.
            tail.push(current.file_name()?.to_owned());
            current = current.parent()?;
        }
    }
}

/// Sandboxed file tools for a coding agent.
pub struct CodeTools {
    config: CodeToolsConfig,
}

impl CodeTools {
    /// Creates a new toolset bound to `config`.
    pub fn new(config: CodeToolsConfig) -> Self {
        Self { config }
    }

    fn resolve(&self, path: &str) -> Result<PathBuf, ToolError> {
        self.config
            .resolve_path(path)
            .ok_or_else(|| ToolError::execution_error(format!("path '{path}' escapes sandbox")))
    }

    /// The canonical sandbox root, which the walking tools report paths
    /// relative to and read ignore files from.
    fn sandbox_root(&self) -> Result<PathBuf, ToolError> {
        self.config.working_dir.canonicalize().map_err(|err| {
            ToolError::execution_error(format!(
                "working directory '{}' is unreadable: {err}",
                self.config.working_dir.display()
            ))
        })
    }
}

#[toolset]
impl CodeTools {
    /// List the entries in a directory (relative to the working directory).
    #[tool]
    async fn list_files(
        &self,
        /// Directory path relative to the working directory.
        path: String,
    ) -> Result<String, ToolError> {
        let resolved = self.resolve(&path)?;
        let mut entries = tokio::fs::read_dir(&resolved)
            .await
            .map_err(|err| io_failure("list_files", &path, &err))?;

        let mut files = Vec::new();
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|err| io_failure("list_files", &path, &err))?
        {
            let suffix = if entry.path().is_dir() { "/" } else { "" };
            files.push(format!("{}{suffix}", entry.file_name().to_string_lossy()));
        }
        files.sort();

        Ok(if files.is_empty() {
            "(empty directory)".to_string()
        } else {
            files.join("\n")
        })
    }

    /// Read a UTF-8 file (relative to the working directory).
    ///
    /// Prefer `outline` + `read_symbol` for Rust sources — they return only the
    /// definition you need. Use `offset`/`limit` to read a line range of a large
    /// or non-Rust file rather than dumping the whole thing; a ranged read is
    /// returned with line numbers. Output is capped at [`MAX_READ_BYTES`]; when
    /// a read stops short of the end of the file, the last line says which
    /// lines you got, how many the file has, and the `offset` to pass here to
    /// continue — no such line means you have the whole file.
    #[tool]
    async fn read_file(
        &self,
        /// File path relative to the working directory.
        path: String,
        /// 1-based line to start at. Omit to start at the top.
        #[default(None::<usize>)]
        offset: Option<usize>,
        /// Maximum number of lines to return. Omit for the rest of the file.
        #[default(None::<usize>)]
        limit: Option<usize>,
    ) -> Result<String, ToolError> {
        let resolved = self.resolve(&path)?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| io_failure("read_file", &path, &err))?;
        // A ranged read is line-numbered so coordinates round-trip to
        // `read_span`/`edit_file`; an unscoped read stays raw to preserve the
        // file's exact bytes.
        if offset.is_some() || limit.is_some() {
            let from = offset.unwrap_or(1);
            return Ok(
                read_view(&path, &content, from, limit, resume_footer).unwrap_or_else(|| {
                    format!(
                        "(no lines: {path} has {} lines, and the requested range starts at {from})",
                        content.lines().count()
                    )
                }),
            );
        }
        if content.len() <= MAX_READ_BYTES {
            return Ok(content);
        }
        Ok(raw_truncated(&path, &content))
    }

    /// Read an inclusive 1-based line range `[start, end]` of a file, returned
    /// with line numbers.
    ///
    /// The coordinate-level escape hatch behind `read_symbol`: when you already
    /// know the span (from `outline`, a search hit, or a prior read) this returns
    /// exactly those lines without the rest of the file riding along in the
    /// window.
    #[tool]
    async fn read_span(
        &self,
        /// File path relative to the working directory.
        path: String,
        /// First line to return (1-based, inclusive).
        start: usize,
        /// Last line to return (1-based, inclusive).
        end: usize,
    ) -> Result<String, ToolError> {
        if start == 0 {
            return Err(ToolError::execution_error(format!(
                "read_span '{path}': start must be >= 1"
            )));
        }
        if end < start {
            return Err(ToolError::execution_error(format!(
                "read_span '{path}': end must be >= start (got {start}-{end})"
            )));
        }
        let resolved = self.resolve(&path)?;
        let content = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| io_failure("read_span", &path, &err))?;
        Ok(
            read_view(&path, &content, start, Some(end - start + 1), cap_footer).unwrap_or_else(
                || {
                    format!(
                        "(no lines: {path} has {} lines, and {start}-{end} is past the end)",
                        content.lines().count()
                    )
                },
            ),
        )
    }

    /// Replace an exact substring in a file and return a unified diff.
    ///
    /// `old_string` must match the file content exactly. Unless `replace_all`
    /// is set, it must match exactly once — otherwise the edit is rejected so
    /// the agent can add surrounding context to disambiguate.
    #[tool]
    async fn edit_file(
        &self,
        /// File path relative to the working directory.
        path: String,
        /// Exact text to replace. Must occur in the file.
        old_string: String,
        /// Replacement text.
        new_string: String,
        /// Replace every occurrence instead of requiring a unique match.
        replace_all: bool,
    ) -> Result<String, ToolError> {
        if old_string == new_string {
            return Err(ToolError::execution_error(format!(
                "edit_file '{path}': old_string and new_string are identical"
            )));
        }
        let resolved = self.resolve(&path)?;
        let original = tokio::fs::read_to_string(&resolved)
            .await
            .map_err(|err| io_failure("edit_file", &path, &err))?;

        let matches = original.matches(&old_string).count();
        if matches == 0 {
            return Err(ToolError::execution_error(format!(
                "old_string not found in {path} (whitespace must match exactly); \
                 read the file and copy the text you mean to replace",
            )));
        }
        if matches > 1 && !replace_all {
            return Err(ToolError::execution_error(format!(
                "old_string is not unique in {path} ({matches} matches); add surrounding \
                 context to disambiguate, or set replace_all = true",
            )));
        }

        let updated = if replace_all {
            original.replace(&old_string, &new_string)
        } else {
            original.replacen(&old_string, &new_string, 1)
        };

        if let Some(journal) = &self.config.edit_journal {
            journal
                .record_edit(
                    &resolved,
                    original.as_bytes().to_vec(),
                    updated.as_bytes().to_vec(),
                )
                .map_err(ToolError::execution_error)?;
        }

        // A same-directory replacement avoids exposing a truncated file if the
        // process is cancelled between write calls. The journal intent is
        // durable before this point.
        let temporary = resolved.with_extension(format!("polaris-edit-{}", std::process::id()));
        let permissions = tokio::fs::metadata(&resolved)
            .await
            .map_err(|err| io_failure("edit_file (stat)", &path, &err))?
            .permissions();
        // A failure after the scratch file exists must remove it. It sits in
        // the repository next to the file being edited, where `git add -A`
        // would collect it into a commit or a prediction patch — and unlike
        // the state directory it cannot be hidden by an ignore rule. Each step
        // still names itself and the file it was acting on.
        let replace = async {
            tokio::fs::write(&temporary, &updated)
                .await
                .map_err(|err| io_failure("edit_file (write)", &path, &err))?;
            tokio::fs::set_permissions(&temporary, permissions)
                .await
                .map_err(|err| io_failure("edit_file (chmod)", &path, &err))?;
            tokio::fs::rename(&temporary, &resolved)
                .await
                .map_err(|err| io_failure("edit_file (replace)", &path, &err))
        };
        if let Err(err) = replace.await {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(err);
        }

        let diff = similar::TextDiff::from_lines(&original, &updated)
            .unified_diff()
            .context_radius(3)
            .header(&path, &path)
            .to_string();

        Ok(format!("Edited {path} ({matches} replacement(s)):\n{diff}"))
    }

    /// Create a new file with the given content, creating any missing parent
    /// directories inside the working directory, and return a unified diff.
    ///
    /// The path must not exist yet: an existing file, directory or link is
    /// left untouched and the call fails — use `edit_file` to change a file
    /// that already exists. The content is written exactly as given; nothing
    /// is added or stripped.
    #[tool]
    async fn create_file(
        &self,
        /// Path of the new file relative to the working directory. Must not
        /// exist yet.
        path: String,
        /// The complete content of the new file.
        content: String,
    ) -> Result<String, ToolError> {
        let resolved = self.resolve(&path)?;
        // `symlink_metadata` sees a dangling link too, so a link at the path
        // is refused rather than followed and created through.
        if tokio::fs::symlink_metadata(&resolved).await.is_ok() {
            return Err(ToolError::execution_error(format!(
                "'{path}' already exists; use edit_file to change an existing file"
            )));
        }

        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|err| ToolError::execution_error(format!("{path}: {err}")))?;
            // `resolve_path` validated a tail that did not exist yet; now that
            // it does, check the real parent again so a link that appeared in
            // between cannot redirect the create outside the sandbox.
            let real_parent = tokio::fs::canonicalize(parent)
                .await
                .map_err(|err| ToolError::execution_error(format!("{path}: {err}")))?;
            let root = self
                .config
                .working_dir
                .canonicalize()
                .map_err(|err| ToolError::execution_error(err.to_string()))?;
            if !real_parent.starts_with(&root) {
                return Err(ToolError::execution_error(format!(
                    "path '{path}' escapes sandbox"
                )));
            }
        }

        // A file that did not exist has an empty pre-image, so a creation is
        // journaled on the same terms as an edit and rewinds to empty bytes.
        // The intent is durable before the file exists, as for `edit_file`.
        if let Some(journal) = &self.config.edit_journal {
            journal
                .record_edit(&resolved, Vec::new(), content.as_bytes().to_vec())
                .map_err(ToolError::execution_error)?;
        }

        // `create_new` is exclusive: it fails if anything appeared at the path
        // since the check above, and it never follows a symlink there, so the
        // no-clobber rule holds at the moment of creation, not just before it.
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&resolved)
            .await
            .map_err(|err| ToolError::execution_error(format!("{path}: {err}")))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|err| ToolError::execution_error(format!("{path}: {err}")))?;
        // A tokio `File` completes its writes on a background thread; `flush`
        // waits for them, so the bytes are on disk before the receipt says so
        // and before anything — the re-indexer, a rewind — reads them back.
        file.flush()
            .await
            .map_err(|err| ToolError::execution_error(format!("{path}: {err}")))?;

        let diff = similar::TextDiff::from_lines("", &content)
            .unified_diff()
            .context_radius(3)
            .header("/dev/null", &path)
            .to_string();

        Ok(cap(
            format!(
                "Created {path} ({} lines, {} bytes):\n{diff}",
                content.lines().count(),
                content.len()
            ),
            MAX_READ_BYTES,
        ))
    }

    /// Search file contents, returning `path:line: text` per match.
    ///
    /// `pattern` is a regular expression in the Rust `regex` dialect:
    /// `.` `[a-z]` `\d` `\w` `\s` `\b`, `*` `+` `?` `{m,n}` (append `?` to make
    /// one lazy), `(…)` groups, `(?:…)` non-capturing, `|` alternation, `^`/`$`
    /// anchored to the line. There are no backreferences and no lookaround.
    /// Escape a metacharacter with `\`, or set `literal = true` to search for
    /// the text exactly as written — that is the right choice for a symbol
    /// containing `(`, `[`, `.`, `?` or `*`. A pattern that does not compile is
    /// reported as an error naming the fault, never as "no matches".
    ///
    /// Narrow the search with `path` (a directory or a single file) and `glob`
    /// (which files under `path` to look in). Files and directories the
    /// repository's own `.gitignore` rules exclude are skipped, along with
    /// `.git`; set `include_ignored = true` to search them anyway.
    ///
    /// Reported paths are relative to the working directory, so they feed
    /// straight back into `read_file`, `read_span` and `edit_file`. Results are
    /// bounded; when a bound applies the tool says so on a trailing `…` line.
    /// For Rust symbols prefer `find_definition` / `find_references` — they
    /// resolve by symbol rather than by text.
    #[tool]
    async fn search(
        &self,
        /// Regular expression to match against each line — or, with
        /// `literal = true`, the exact text to find.
        pattern: String,
        /// Directory or file to search under, relative to the working
        /// directory. Omit to search the whole working directory.
        #[default(None::<String>)]
        path: Option<String>,
        /// Which files under `path` to search, as a `.gitignore`-style
        /// pattern: `*.rs` matches that name at any depth, `src/**/*.rs` is
        /// anchored to `path`, `tests/` takes a whole subtree. `*` and `?`
        /// stay inside one path component; `**` crosses them. Omit to search
        /// every file.
        #[default(None::<String>)]
        glob: Option<String>,
        /// Treat `pattern` as exact text instead of a regular expression.
        #[default(false)]
        literal: bool,
        /// Match regardless of letter case.
        #[default(false)]
        case_insensitive: bool,
        /// Also search what the repository's `.gitignore` rules exclude.
        #[default(false)]
        include_ignored: bool,
    ) -> Result<String, ToolError> {
        let target = path.unwrap_or_else(|| ".".to_string());
        let root = self.resolve(&target)?;
        let sandbox = self.sandbox_root()?;
        let matcher = compile_matcher(&pattern, literal, case_insensitive)?;
        let selector = glob
            .as_deref()
            .map(PathGlob::compile)
            .transpose()
            .map_err(ToolError::execution_error)?;
        let described = describe_query(&pattern, literal, case_insensitive);

        tokio::task::spawn_blocking(move || {
            let selection = select_files(
                &sandbox,
                &root,
                selector.as_ref(),
                !include_ignored,
                WalkLimits {
                    max_files: MAX_SEARCH_FILES,
                    max_file_bytes: MAX_SEARCH_FILE_BYTES,
                },
            );
            render_search(
                &sandbox,
                &selection,
                &matcher,
                selector.as_ref(),
                include_ignored,
                &described,
                &target,
            )
        })
        .await
        .map_err(|err| ToolError::execution_error(format!("search was interrupted: {err}")))
    }

    /// Find files by path pattern, returning one path per line.
    ///
    /// `pattern` is a `.gitignore`-style path pattern: `*.rs` matches that
    /// name at any depth, `src/**/*.rs` is anchored to `path`, `tests/` takes
    /// everything under a directory. `*` and `?` stay inside one path
    /// component, `**` crosses them, and `[a-z]` is a character class.
    ///
    /// Paths the repository's own `.gitignore` rules exclude are omitted,
    /// along with `.git`; set `include_ignored = true` to list them anyway.
    /// Results are relative to the working directory, sorted by path, and
    /// bounded — when a bound applies the tool says so on a trailing `…` line.
    #[tool]
    async fn glob(
        &self,
        /// `.gitignore`-style path pattern, e.g. `*.toml`, `src/**/*.rs`,
        /// `tests/`.
        pattern: String,
        /// Directory to look under, relative to the working directory. Omit to
        /// search the whole working directory.
        #[default(None::<String>)]
        path: Option<String>,
        /// Also list what the repository's `.gitignore` rules exclude.
        #[default(false)]
        include_ignored: bool,
    ) -> Result<String, ToolError> {
        let target = path.unwrap_or_else(|| ".".to_string());
        let root = self.resolve(&target)?;
        let sandbox = self.sandbox_root()?;
        let selector = PathGlob::compile(&pattern).map_err(ToolError::execution_error)?;

        tokio::task::spawn_blocking(move || {
            let selection = select_files(
                &sandbox,
                &root,
                Some(&selector),
                !include_ignored,
                WalkLimits {
                    max_files: MAX_GLOB_RESULTS,
                    max_file_bytes: u64::MAX,
                },
            );
            if selection.files.is_empty() {
                return no_files_message(selector.pattern(), &target, include_ignored);
            }
            let mut listed: Vec<String> = selection
                .files
                .iter()
                .map(|file| slashed(file.strip_prefix(&sandbox).unwrap_or(file)))
                .collect();
            listed.sort();
            let mut out = listed.join("\n");
            if selection.stopped_at_limit {
                out.push_str(&format!(
                    "\n… stopped at {MAX_GLOB_RESULTS} paths — there are more; \
                     narrow the pattern or pass a deeper path"
                ));
            }
            cap(out, MAX_READ_BYTES)
        })
        .await
        .map_err(|err| ToolError::execution_error(format!("glob was interrupted: {err}")))
    }
}

/// Compiles a caller-supplied pattern, or explains what is wrong with it.
///
/// A literal is escaped rather than run through a different code path, so the
/// literal and pattern cases cannot drift apart: `literal = true` searches for
/// exactly the bytes given, metacharacters included.
fn compile_matcher(
    pattern: &str,
    literal: bool,
    case_insensitive: bool,
) -> Result<Regex, ToolError> {
    if pattern.is_empty() {
        return Err(ToolError::execution_error(
            "pattern is empty; pass the text or regular expression to search for",
        ));
    }
    let source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    RegexBuilder::new(&source)
        .case_insensitive(case_insensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|err| {
            ToolError::execution_error(format!(
                "'{pattern}' is not a valid regular expression: {err}\n\
                 Set literal = true to search for this text exactly as written."
            ))
        })
}

/// How a query is named back to the caller in a result line.
fn describe_query(pattern: &str, literal: bool, case_insensitive: bool) -> String {
    let fold = if case_insensitive {
        ", case-insensitive"
    } else {
        ""
    };
    if literal {
        format!("the literal '{pattern}'{fold}")
    } else {
        format!("pattern /{pattern}/{fold}")
    }
}

/// The result `search` and `glob` return when the selection was empty — the
/// one case where an unhelpful "no matches" hides a fixable mistake.
fn no_files_message(pattern: &str, target: &str, include_ignored: bool) -> String {
    let mut out = format!("no files under '{target}' match glob '{pattern}'");
    if !include_ignored {
        out.push_str(
            "\n… ignored files were not considered; set include_ignored = true to include them",
        );
    }
    out
}

/// Runs `matcher` over `selection` and renders the bounded result.
fn render_search(
    sandbox: &Path,
    selection: &Selection,
    matcher: &Regex,
    glob: Option<&PathGlob>,
    include_ignored: bool,
    described: &str,
    target: &str,
) -> String {
    let mut hits: Vec<String> = Vec::new();
    let mut files_with_matches = 0usize;
    let mut searched = 0usize;
    let mut unreadable = 0usize;
    let mut capped = false;

    'files: for file in &selection.files {
        let Ok(content) = std::fs::read_to_string(file) else {
            // Non-UTF-8 content is binary as far as a line search is concerned.
            unreadable += 1;
            continue;
        };
        searched += 1;
        let display = slashed(file.strip_prefix(sandbox).unwrap_or(file));
        let before = hits.len();
        for (index, line) in content.lines().enumerate() {
            if !matcher.is_match(line) {
                continue;
            }
            hits.push(format!(
                "{display}:{}: {}",
                index + 1,
                clip(line.trim(), MAX_MATCH_LINE_CHARS)
            ));
            if hits.len() >= MAX_SEARCH_MATCHES {
                capped = true;
                files_with_matches += 1;
                break 'files;
            }
        }
        if hits.len() > before {
            files_with_matches += 1;
        }
    }

    if hits.is_empty() {
        if selection.files.is_empty()
            && let Some(glob) = glob
        {
            return no_files_message(glob.pattern(), target, include_ignored);
        }
        let mut out = format!("no matches for {described} in {searched} file(s) under '{target}'");
        if !include_ignored {
            out.push_str(
                "\n… ignored files were not searched; set include_ignored = true to search them",
            );
        }
        return out;
    }

    let mut out = hits.join("\n");
    if capped {
        out.push_str(&format!(
            "\n… stopped at {MAX_SEARCH_MATCHES} matches — there are more; narrow with path, \
             glob, or a more specific pattern"
        ));
    } else {
        out.push_str(&format!(
            "\n… {} match(es) in {files_with_matches} of {searched} file(s)",
            hits.len()
        ));
    }
    if selection.stopped_at_limit {
        out.push_str(&format!(
            "\n… stopped after {MAX_SEARCH_FILES} files; narrow with path or glob"
        ));
    }
    if selection.skipped_large > 0 {
        out.push_str(&format!(
            "\n… {} file(s) over {} KiB were not searched",
            selection.skipped_large,
            MAX_SEARCH_FILE_BYTES / 1024
        ));
    }
    if unreadable > 0 {
        out.push_str(&format!("\n… {unreadable} non-text file(s) were skipped"));
    }
    cap(out, MAX_READ_BYTES)
}

/// Shorten one echoed match line, on a char boundary.
fn clip(line: &str, limit: usize) -> String {
    if line.chars().count() <= limit {
        return line.to_string();
    }
    let kept: String = line.chars().take(limit).collect();
    format!("{kept}… (line truncated)")
}

/// Plugin that registers [`CodeTools`] with the [`ToolRegistry`].
///
/// `edit_file` and `create_file` — the toolset's two mutating tools — are
/// marked [`ToolPermission::Confirm`] so the agent host can gate every write
/// to the tree behind user approval.
///
/// # Resources Provided
///
/// | Resource | Scope | Description |
/// |----------|-------|-------------|
/// | (none)   | —     | Registers tools on the existing `ToolRegistry`; provides no new resource. |
///
/// # Dependencies
///
/// - [`ToolsPlugin`] — provides the [`ToolRegistry`] this plugin populates.
///
/// # Example
///
/// ```no_run
/// use polaris::system::server::Server;
/// use polaris::tools::ToolsPlugin;
/// use polaris_code::{CodeToolsConfig, CodeToolsPlugin};
///
/// let mut server = Server::new();
/// server
///     .add_plugins(ToolsPlugin::default())
///     .add_plugins(CodeToolsPlugin::new(CodeToolsConfig::new(".")));
/// ```
pub struct CodeToolsPlugin {
    config: CodeToolsConfig,
}

impl CodeToolsPlugin {
    /// Creates a plugin that registers tools sandboxed to `config.working_dir`.
    pub fn new(config: CodeToolsConfig) -> Self {
        Self { config }
    }
}

impl Plugin for CodeToolsPlugin {
    const ID: &'static str = "polaris_code::code_tools";
    const VERSION: Version = Version::new(0, 1, 0);

    fn dependencies(&self) -> Vec<PluginId> {
        vec![PluginId::of::<ToolsPlugin>()]
    }

    fn build(&self, server: &mut Server) {
        let mut registry = server
            .get_resource_mut::<ToolRegistry>()
            .expect("ToolsPlugin must be added before CodeToolsPlugin");
        registry.register_toolset(CodeTools::new(self.config.clone()));
        for tool in FILE_WRITE_TOOLS {
            registry
                .set_permission(tool, ToolPermission::Confirm)
                .unwrap_or_else(|_| panic!("{tool} should be registered by CodeTools toolset"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn edit_file_unique_match_applies_and_diffs() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "hello\nworld\n").await.unwrap();

        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let out = tools
            .edit_file("a.txt".into(), "world".into(), "polaris".into(), false)
            .await
            .unwrap();

        assert!(out.contains("+polaris") || out.contains("polaris"));
        let content = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(content, "hello\npolaris\n");
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "x\nx\n").await.unwrap();

        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let err = tools
            .edit_file("a.txt".into(), "x".into(), "y".into(), false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not unique"));
    }

    #[tokio::test]
    async fn create_file_writes_exact_bytes_and_returns_a_diff() {
        let dir = tempfile::tempdir().unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let out = tools
            .create_file("notes.md".into(), "# notes\n\nbody\n".into())
            .await
            .unwrap();

        assert!(
            out.starts_with("Created notes.md (3 lines, 14 bytes):"),
            "{out}"
        );
        assert!(out.contains("--- /dev/null"), "{out}");
        assert!(out.contains("+++ notes.md"), "{out}");
        assert!(out.contains("+# notes"), "{out}");
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("notes.md"))
                .await
                .unwrap(),
            "# notes\n\nbody\n"
        );
    }

    #[tokio::test]
    async fn create_file_makes_missing_parents_inside_the_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        tools
            .create_file("a/b/c.txt".into(), "deep\n".into())
            .await
            .unwrap();

        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("a/b/c.txt"))
                .await
                .unwrap(),
            "deep\n"
        );
    }

    #[tokio::test]
    async fn create_file_refuses_an_existing_file_directory_or_link() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("keep.txt"), "original")
            .await
            .unwrap();
        tokio::fs::create_dir(dir.path().join("dir")).await.unwrap();
        // A dangling link: `metadata` would say it does not exist, and a
        // follow-through create would write to wherever it points.
        std::os::unix::fs::symlink("missing-target", dir.path().join("dangling")).unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        for path in ["keep.txt", "dir", "dangling"] {
            let err = tools
                .create_file(path.into(), "x".into())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("already exists"), "{path}: {err}");
            assert!(err.contains("edit_file"), "{path}: {err}");
        }
        assert_eq!(
            tokio::fs::read_to_string(dir.path().join("keep.txt"))
                .await
                .unwrap(),
            "original"
        );
        assert!(!dir.path().join("missing-target").exists());
    }

    #[tokio::test]
    async fn create_file_refuses_the_same_escapes_as_edit_file() {
        let dir = tempfile::tempdir().unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        for path in [
            "../escape.txt",
            "new/../../escape.txt",
            "/etc/polaris-escape",
        ] {
            let create = tools
                .create_file(path.into(), "x".into())
                .await
                .unwrap_err()
                .to_string();
            let edit = tools
                .edit_file(path.into(), "a".into(), "b".into(), false)
                .await
                .unwrap_err()
                .to_string();
            assert!(create.contains("escapes sandbox"), "{path}: {create}");
            assert_eq!(create, edit, "{path}: one rule, one refusal");
        }
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
    }

    #[tokio::test]
    async fn create_file_refuses_a_parent_linked_outside_the_sandbox() {
        // A symlinked directory *inside* the tree pointing *outside* it: the
        // resolver canonicalizes through the link and lands outside the root.
        let outside = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let err = tools
            .create_file("out/new.txt".into(), "x".into())
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("escapes sandbox"), "{err}");
        assert!(!outside.path().join("new.txt").exists());
    }

    #[tokio::test]
    async fn create_file_records_an_empty_pre_image_in_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let journal = EditJournal::open(&root).unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(&root).with_edit_journal(journal.clone()));
        let point = journal
            .add_point(
                &polaris::sessions::SessionId::from_string("s"),
                Default::default(),
                serde_json::from_str("1").unwrap(),
                "before".into(),
            )
            .unwrap();

        tools
            .create_file("new.txt".into(), "created\n".into())
            .await
            .unwrap();

        assert_eq!(journal.affected_paths(&point), vec![root.join("new.txt")]);
        // The pre-image of a file that did not exist is empty: a rewind takes
        // the file back to zero bytes, which is what the journal can promise.
        let outcomes = journal.restore_edits(&point);
        assert!(
            matches!(
                outcomes.as_slice(),
                [crate::terminal::PathOutcome::Restored(_)]
            ),
            "{outcomes:?}"
        );
        assert_eq!(
            tokio::fs::read_to_string(root.join("new.txt"))
                .await
                .unwrap(),
            ""
        );
    }

    #[tokio::test]
    async fn read_file_truncates_large_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        tokio::fs::write(&file, "x".repeat(MAX_READ_BYTES * 2))
            .await
            .unwrap();

        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let out = tools.read_file("big.txt".into(), None, None).await.unwrap();
        assert!(out.contains("truncated at"), "expected truncation marker");
        assert!(out.len() < MAX_READ_BYTES * 2);
    }

    #[tokio::test]
    async fn read_span_returns_inclusive_numbered_range() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "one\ntwo\nthree\nfour\n")
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let out = tools.read_span("a.rs".into(), 2, 3).await.unwrap();
        assert!(out.contains("2\ttwo"), "got: {out}");
        assert!(out.contains("3\tthree"), "got: {out}");
        assert!(!out.contains("one"), "range must exclude line 1: {out}");
        assert!(!out.contains("four"), "range must exclude line 4: {out}");
    }

    #[tokio::test]
    async fn read_span_rejects_inverted_range() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "x\ny\n")
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        assert!(
            tools
                .read_span("a.rs".into(), 3, 1)
                .await
                .unwrap_err()
                .to_string()
                .contains("end must be >= start")
        );
    }

    #[tokio::test]
    async fn read_file_range_is_numbered_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        tokio::fs::write(dir.path().join("a.txt"), body)
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let out = tools
            .read_file("a.txt".into(), Some(4), Some(2))
            .await
            .unwrap();
        assert!(out.contains("4\tline4"), "got: {out}");
        assert!(out.contains("5\tline5"), "got: {out}");
        assert!(!out.contains("line3"), "got: {out}");
        assert!(!out.contains("\tline6"), "got: {out}");
        // The gutter is padded to this window's widest number, not to six.
        assert!(!out.contains("   4\t"), "gutter is not over-padded: {out}");
    }

    #[tokio::test]
    async fn read_file_without_range_is_raw() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n")
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let out = tools.read_file("a.txt".into(), None, None).await.unwrap();
        assert_eq!(out, "alpha\nbeta\n");
    }

    /// The line number a resumption footer points at, or a failure naming what
    /// the result actually said. Parsing it the way a model would read it is
    /// the point: a footer that cannot be acted on is not a footer.
    fn next_offset(result: &str) -> usize {
        let footer = result
            .lines()
            .last()
            .unwrap_or_else(|| panic!("empty result"));
        let marker = "offset=";
        let at = footer
            .find(marker)
            .unwrap_or_else(|| panic!("no resumption offset in: {footer}"));
        footer[at + marker.len()..]
            .trim_end_matches(|c: char| !c.is_ascii_digit())
            .parse()
            .unwrap_or_else(|err| panic!("unparsable offset in {footer}: {err}"))
    }

    /// A file whose unscoped read cannot fit the cap: 5000 numbered lines.
    async fn long_file(dir: &std::path::Path) -> usize {
        let body: String = (1..=5000).map(|n| format!("line{n}\n")).collect();
        assert!(body.len() > MAX_READ_BYTES);
        tokio::fs::write(dir.join("long.txt"), body).await.unwrap();
        5000
    }

    #[tokio::test]
    async fn a_truncated_read_states_the_total_and_resumes_where_it_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let total = long_file(dir.path()).await;
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let head = tools
            .read_file("long.txt".into(), None, None)
            .await
            .unwrap();
        assert!(
            head.contains("truncated"),
            "a cut result must say it was cut: {}",
            head.lines().last().unwrap()
        );
        assert!(
            head.contains(&format!("of {total}")),
            "a cut result must say how much exists: {}",
            head.lines().last().unwrap()
        );

        // The stated resumption point is exact: following it returns the very
        // next line, with nothing skipped and nothing repeated.
        let offset = next_offset(&head);
        assert!(
            head.contains(&format!("line{}\n", offset - 1)),
            "the last shown line must be the one before the next offset"
        );
        assert!(
            !head.contains(&format!("line{offset}\t"))
                && !head.contains(&format!("\nline{offset}")),
            "the next offset must not already be in the result"
        );
        let rest = tools
            .read_file("long.txt".into(), Some(offset), Some(1))
            .await
            .unwrap();
        assert!(
            rest.contains(&format!("\tline{offset}")),
            "resuming at the stated offset must return line {offset}: {rest}"
        );
    }

    #[tokio::test]
    async fn a_ranged_read_short_of_the_end_says_what_to_call_next() {
        let dir = tempfile::tempdir().unwrap();
        let total = long_file(dir.path()).await;
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let page = tools
            .read_file("long.txt".into(), Some(10), Some(5))
            .await
            .unwrap();
        assert!(
            page.contains(&format!("of {total}")),
            "the model must learn the file is longer than its window: {page}"
        );
        assert_eq!(next_offset(&page), 15, "got: {page}");

        // A window that reaches the end says nothing extra — silence means
        // "that was all of it", so the footer stays informative.
        let tail = tools
            .read_file("long.txt".into(), Some(4999), None)
            .await
            .unwrap();
        assert!(!tail.contains("offset="), "no footer at EOF: {tail}");
    }

    /// The notice has to be paid for out of the same result, not added to it.
    ///
    /// The renderer pads to the width the window needs and spends less than a
    /// fixed six-column gutter on a line that says what to call next.
    #[tokio::test]
    async fn the_resumption_notice_costs_less_than_the_padding_it_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=250)
            .map(|n| format!("    let value_{n} = compute({n});\n"))
            .collect();
        tokio::fs::write(dir.path().join("wide.rs"), &body)
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let window = tools
            .read_file("wide.rs".into(), Some(100), Some(60))
            .await
            .unwrap();
        // Comparison cost: a fixed six-column gutter, a tab, the line, a
        // newline, and no notice at the end.
        let before: usize = body
            .lines()
            .skip(99)
            .take(60)
            .map(|line| 6 + 1 + line.len() + 1)
            .sum();

        assert_eq!(before, 2460);
        assert_eq!(window.len(), 2356);
        assert!(
            window.contains("of 250") && window.contains("offset=160"),
            "and it still says everything: {}",
            window.lines().last().unwrap()
        );
    }

    #[tokio::test]
    async fn a_failing_read_names_the_path_it_failed_on() {
        let dir = tempfile::tempdir().unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        for message in [
            tools
                .read_file("src/missing.rs".into(), None, None)
                .await
                .unwrap_err()
                .to_string(),
            tools
                .read_span("src/missing.rs".into(), 1, 5)
                .await
                .unwrap_err()
                .to_string(),
            tools
                .edit_file("src/missing.rs".into(), "a".into(), "b".into(), false)
                .await
                .unwrap_err()
                .to_string(),
            tools
                .list_files("src/missing".into())
                .await
                .unwrap_err()
                .to_string(),
        ] {
            assert!(
                message.contains("src/missing"),
                "a failure must name what it operated on: {message}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_edit_names_the_file_it_examined() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.txt"), "x\nx\n")
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let ambiguous = tools
            .edit_file("a.txt".into(), "x".into(), "y".into(), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(ambiguous.contains("a.txt"), "got: {ambiguous}");

        let absent = tools
            .edit_file("a.txt".into(), "zzz".into(), "y".into(), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(absent.contains("a.txt"), "got: {absent}");
    }

    #[tokio::test]
    async fn a_capped_search_says_how_many_of_how_many_and_how_to_narrow() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=400).map(|n| format!("needle {n}\n")).collect();
        tokio::fs::write(dir.path().join("hay.txt"), body)
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));

        let out = tools
            .search("needle".into(), None, None, true, false, false)
            .await
            .unwrap();
        let footer = out.lines().last().unwrap();
        assert!(
            footer.contains("200"),
            "the cap must say how many matches it stopped at: {footer}"
        );
        assert!(
            footer.contains("narrow"),
            "the cap must say what to do next: {footer}"
        );
    }

    #[tokio::test]
    async fn an_empty_search_names_the_directory_it_looked_in() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir(dir.path().join("src")).await.unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let out = tools
            .search(
                "needle".into(),
                Some("src".into()),
                None,
                true,
                false,
                false,
            )
            .await
            .unwrap();
        assert!(out.contains("src"), "got: {out}");
    }

    #[test]
    fn resolve_path_blocks_escape() {
        let dir = tempfile::tempdir().unwrap();
        let config = CodeToolsConfig::new(dir.path());
        assert!(config.resolve_path("../../etc/passwd").is_none());
        assert!(config.resolve_path("notes.md").is_some());
    }

    #[test]
    fn resolve_path_accepts_new_file_in_new_subdir() {
        // The not-yet-existing branch must accept a file (and parent dirs) that
        // don't exist yet, as long as the resolved location stays in the sandbox.
        let dir = tempfile::tempdir().unwrap();
        let config = CodeToolsConfig::new(dir.path());
        let resolved = config
            .resolve_path("newdir/new.txt")
            .expect("a new file under a new subdir is inside the sandbox");
        assert!(resolved.starts_with(dir.path().canonicalize().unwrap()));
    }

    #[test]
    fn resolve_path_rejects_escape_through_a_nonexistent_leaf() {
        // `..` segments in a not-yet-existing path must not slip past the check:
        // the *resolved* destination lands outside the sandbox.
        let dir = tempfile::tempdir().unwrap();
        let config = CodeToolsConfig::new(dir.path());
        assert!(config.resolve_path("nope/../../escape.txt").is_none());
    }

    #[test]
    fn line_view_past_eof_is_empty() {
        // An offset past the last line emits nothing (the empty-range fallback
        // `read_span` turns into a "(no lines …)" message).
        let past = line_view("one\ntwo\n", 5, Some(3), MAX_READ_BYTES);
        assert_eq!(past.body, "");
        assert_eq!(past.last, None);
        assert_eq!(past.total, 2);
        // A zero limit likewise emits nothing.
        assert_eq!(line_view("one\ntwo\n", 1, Some(0), MAX_READ_BYTES).body, "");
    }

    #[test]
    fn a_line_wider_than_the_budget_still_yields_a_marked_partial_line() {
        // The pathological minified-source case: there is no line coordinate
        // that resumes mid-line, so the view must at least say it is partial
        // rather than present half a line as a whole one.
        let content = format!("{}\nsecond\n", "x".repeat(100));
        let view = line_view(&content, 1, None, 20);
        assert_eq!(view.last, Some(1));
        assert!(view.capped && view.partial, "{view:?}");
        assert!(view.body.len() <= 20, "{}", view.body.len());
        let footer = resume_footer("min.js", &view).expect("a partial view has more to give");
        assert!(footer.contains("mid-line 1"), "got: {footer}");
    }

    #[tokio::test]
    async fn read_span_past_eof_reports_empty_range() {
        let dir = tempfile::tempdir().unwrap();
        tokio::fs::write(dir.path().join("a.rs"), "one\ntwo\n")
            .await
            .unwrap();
        let tools = CodeTools::new(CodeToolsConfig::new(dir.path()));
        let out = tools.read_span("a.rs".into(), 10, 12).await.unwrap();
        // An empty range says how long the file actually is, so the retry is
        // a corrected range rather than another guess.
        assert!(out.contains("a.rs has 2 lines"), "got: {out}");
        assert!(out.contains("10-12"), "got: {out}");
    }

    #[test]
    fn definitions_expose_the_documented_tool_surface() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = ToolRegistry::new();
        registry.register_toolset(CodeTools::new(CodeToolsConfig::new(dir.path())));
        let defs = registry.definitions();

        let names: std::collections::BTreeSet<&str> =
            defs.iter().map(|d| d.name.as_str()).collect();
        for expected in [
            "read_file",
            "read_span",
            "edit_file",
            "create_file",
            "search",
            "list_files",
        ] {
            assert!(
                names.contains(expected),
                "missing tool {expected}: {names:?}"
            );
        }

        // create_file's schema declares its two documented parameters, and its
        // description tells a model what to reach for when the path exists.
        let create_file = defs.iter().find(|d| d.name == "create_file").unwrap();
        let props = &create_file.parameters["properties"];
        assert!(props.get("path").is_some(), "{:?}", create_file.parameters);
        assert!(
            props.get("content").is_some(),
            "{:?}",
            create_file.parameters
        );
        assert!(
            create_file.description.contains("must not exist yet")
                && create_file.description.contains("edit_file"),
            "{}",
            create_file.description
        );

        // read_span's schema declares its three documented parameters.
        let read_span = defs.iter().find(|d| d.name == "read_span").unwrap();
        let props = &read_span.parameters["properties"];
        assert!(
            props.get("path").is_some(),
            "schema: {:?}",
            read_span.parameters
        );
        assert!(props.get("start").is_some());
        assert!(props.get("end").is_some());
    }
}
