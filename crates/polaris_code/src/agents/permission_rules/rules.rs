use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use polaris::shell::{ShellConfig, ShellExecutor, ShellPermission};
use polaris::system::resource::GlobalResource;
use polaris_cosmos::contracts::ContentDigest;
use serde_json::Value;

use crate::project_trust::{MAX_FILE_BYTES, ProjectTrust, Subject, TrustError, read_content};

/// Most restrictive matching rule wins, independently of file order.
///
/// The variants are declared from least to most restrictive: the derived
/// [`Ord`] is what [`PermissionRules::resolve`] takes the maximum of, so
/// reordering them changes which rule wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleDecision {
    /// Run without a prompt, within the operator's permission ceiling.
    Allow,
    /// Ask a human, even if another rule or grant would allow the call.
    Ask,
    /// Refuse the call.
    Deny,
}

/// One tool-specific rule from a permission file.
///
/// Command selectors are mutually exclusive. `command_prefix` matches a whole
/// command word boundary; `command_glob` uses the shell's `*`/`?` dialect.
/// `path` uses that same dialect against canonical workspace-relative paths.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    /// Exact tool name; `*` matches every tool.
    pub tool: String,
    /// Rule disposition.
    pub decision: RuleDecision,
    /// Literal whole command, including whitespace and quoting.
    pub command: Option<String>,
    /// Literal command prefix ending at a word boundary.
    pub command_prefix: Option<String>,
    /// Shell command wildcard pattern.
    pub command_glob: Option<String>,
    /// Workspace-relative path wildcard pattern for file tools.
    pub path: Option<String>,
    /// Origin, populated by the loader rather than accepted from TOML.
    pub source: PathBuf,
    project: bool,
}

/// Immutable, versioned permission-file snapshot.
#[derive(Debug, Clone, Default)]
pub struct RuleSnapshot {
    /// Parsed rules in source order, with source paths attached.
    pub rules: Vec<PermissionRule>,
    /// File errors; any error makes authorization fail closed.
    pub errors: Vec<String>,
    /// Snapshot revision, bound into exact-call receipts.
    pub revision: u64,
    /// Canonical root against which path selectors resolve.
    pub root: PathBuf,
}

/// Rule files shared by all sessions, provided by [`super::PermissionRulesPlugin`].
///
/// A snapshot is immutable after startup. Only project trust can change, through
/// an explicit local command; it never changes the file contents. Systems read
/// this resource before any call, including calls in the concurrent read lane.
///
/// Project allow rules ask until the user trusts the project's rule file. That
/// decision is kept per project and covers exactly the file text that was
/// loaded (see [`with_project_trust`](Self::with_project_trust)), so editing the
/// file withdraws it.
///
/// # Scope
///
/// Global: one set of rule files governs every session of the process, and the
/// project-trust decision made in one session applies to all of them.
///
/// # Provided By
///
/// [`PermissionRulesPlugin`](super::PermissionRulesPlugin) loads the files and
/// inserts it during `build`. The plugin's default holds no rules.
///
/// # Access
///
/// Read with `Res<PermissionRules>` only. The parsed snapshot is immutable and
/// shared by clones. The one thing that can change is the project-trust
/// decision, held behind interior mutability and changed only by the explicit
/// `/permissions trust-project` command, so no system mutates it through the
/// resource.
///
/// # Alternatives
///
/// [`SessionGrants`](super::SessionGrants) holds what a person approved during
/// this session, [`RunToolGrants`](super::RunToolGrants) what the launch command
/// granted an unattended run, and [`ShellApproval`](super::ShellApproval) the
/// single-use authorization for an approved shell call. Rules decide before any
/// of them.
///
/// # Example
///
/// ```
/// use polaris::system::param::Res;
/// use polaris::system::system;
/// use polaris_code::agents::permission_rules::{PermissionRules, RuleDecision};
///
/// #[system]
/// async fn env_file_is_denied(rules: Res<PermissionRules>) -> bool {
///     let args = serde_json::json!({ "path": ".env" });
///     matches!(rules.resolve("read_file", &args), Some((RuleDecision::Deny, _)))
/// }
/// ```
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    snapshot: Arc<RuleSnapshot>,
    project_trusted: Arc<AtomicBool>,
    project_digest: Option<ContentDigest>,
    trust: ProjectTrust,
}

impl GlobalResource for PermissionRules {}

impl PermissionRules {
    /// Read explicit paths. A missing file contributes no rules; malformed,
    /// unreadable, oversized (over 1 MiB) or non-regular files record errors and
    /// prevent all tool authorization.
    #[must_use]
    pub fn from_paths(root: &Path, user: Option<&Path>, project: Option<&Path>) -> Self {
        let mut snapshot = RuleSnapshot {
            root: root.canonicalize().unwrap_or_else(|_| root.to_path_buf()),
            revision: 1,
            ..RuleSnapshot::default()
        };
        let mut project_digest = None;
        for (path, is_project) in user
            .into_iter()
            .map(|p| (p, false))
            .chain(project.into_iter().map(|p| (p, true)))
        {
            let content = match read_content(path, MAX_FILE_BYTES) {
                Ok(Some(content)) => content,
                Ok(None) => continue,
                Err(err) => {
                    snapshot.errors.push(err.to_string());
                    continue;
                }
            };
            if is_project {
                project_digest = Some(content.digest);
            }
            let Ok(text) = String::from_utf8(content.bytes) else {
                snapshot
                    .errors
                    .push(format!("{}: not valid UTF-8", path.display()));
                continue;
            };
            // A revision is an identity for this process, not a cryptographic digest.
            // The snapshot cannot be reloaded or silently changed after approval.
            let parsed = parse_rules(&text);
            match parsed {
                Ok(file) => {
                    for mut rule in file {
                        rule.source = path.to_path_buf();
                        rule.project = is_project;
                        if let Err(err) = rule.validate() {
                            snapshot.errors.push(format!("{}: {err}", path.display()));
                        } else {
                            snapshot.rules.push(rule);
                        }
                    }
                }
                Err(err) => snapshot.errors.push(format!("{}: {err}", path.display())),
            }
        }
        Self {
            snapshot: Arc::new(snapshot),
            project_trusted: Arc::new(AtomicBool::new(false)),
            project_digest,
            trust: ProjectTrust::default(),
        }
    }

    /// Use the user's saved trust decisions for the project being opened.
    ///
    /// Records the digest of the project rule file that was just loaded, so
    /// [`remember_project_trust`](Self::remember_project_trust) trusts exactly
    /// that text. When the user already trusted that text, the project's allow
    /// rules apply from the start; any other text leaves them asking.
    #[must_use]
    pub fn with_project_trust(mut self, trust: ProjectTrust) -> Self {
        if let Some(digest) = self.project_digest {
            trust.observe(Subject::PermissionRules, digest);
            if trust.is_trusted(&Subject::PermissionRules, digest) {
                self.project_trusted.store(true, Ordering::SeqCst);
            }
        }
        self.trust = trust;
        self
    }

    pub(super) fn discover(root: &Path) -> Self {
        let user = crate::config_dir::config_file("permissions.toml");
        let repo = root
            .ancestors()
            .find(|dir| dir.join(".git").exists())
            .unwrap_or(root);
        Self::from_paths(
            root,
            user.as_deref(),
            Some(&repo.join("polaris.permissions.toml")),
        )
    }

    /// Current immutable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Arc<RuleSnapshot> {
        self.snapshot.clone()
    }

    /// Revision including the explicit project-trust decision.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.snapshot.revision + u64::from(self.project_trusted.load(Ordering::SeqCst))
    }

    /// Enable the already loaded project allow rules after explicit inspection.
    ///
    /// This lasts for the process. [`remember_project_trust`](Self::remember_project_trust)
    /// keeps the decision for later sessions.
    pub fn trust_project(&self) {
        self.project_trusted.store(true, Ordering::SeqCst);
    }

    /// Save the user's trust in the project content this process loaded, so
    /// later sessions apply it without asking again.
    ///
    /// Covers the project's permission rule file and its startup settings file,
    /// each only if one was loaded, and only as it was loaded. Returns what was
    /// saved.
    ///
    /// # Errors
    ///
    /// The first failure to save, for example [`TrustError::NoStore`] when no
    /// user configuration directory exists.
    pub fn remember_project_trust(&self) -> Result<Vec<Subject>, TrustError> {
        let mut saved = Vec::new();
        for subject in [Subject::Settings, Subject::PermissionRules] {
            match self.trust.grant_observed(&subject) {
                Ok(_) => saved.push(subject),
                Err(TrustError::NotObserved(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(saved)
    }

    /// Most restrictive rule matching this exact call, and its source text.
    #[must_use]
    pub fn resolve(&self, tool: &str, args: &Value) -> Option<(RuleDecision, String)> {
        if !self.snapshot.errors.is_empty() {
            return Some((RuleDecision::Deny, self.snapshot.errors.join("\n")));
        }
        self.snapshot
            .rules
            .iter()
            .filter(|rule| rule.matches(tool, args, &self.snapshot.root))
            .map(|rule| {
                let decision = if self.asks_until_trusted(rule) {
                    RuleDecision::Ask
                } else {
                    rule.decision
                };
                (decision, rule)
            })
            .max_by_key(|(decision, _)| *decision)
            .map(|(decision, rule)| (decision, rule.describe()))
    }

    /// Whether a project allow rule is still waiting for project trust.
    fn asks_until_trusted(&self, rule: &PermissionRule) -> bool {
        rule.project
            && rule.decision == RuleDecision::Allow
            && !self.project_trusted.load(Ordering::SeqCst)
    }

    /// All active rules and source paths, suitable for `/permissions`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut lines = vec![format!("Permission rules (revision {}):", self.revision())];
        for rule in &self.snapshot.rules {
            let trust = if self.asks_until_trusted(rule) {
                " [asks until /permissions trust-project]"
            } else {
                ""
            };
            lines.push(format!("- {}{trust}", rule.describe()));
        }
        if self.snapshot.rules.is_empty() {
            lines.push("No file rules.".into());
        }
        lines.extend(
            self.snapshot
                .errors
                .iter()
                .map(|error| format!("ERROR (tools refused): {error}")),
        );
        lines.join("\n")
    }
}

impl PermissionRule {
    fn validate(&self) -> Result<(), String> {
        if self.tool.trim().is_empty() {
            return Err("rule tool cannot be empty".into());
        }
        for pattern in [&self.path, &self.command_glob].into_iter().flatten() {
            glob::Pattern::new(pattern)
                .map_err(|err| format!("invalid wildcard pattern: {err}"))?;
        }
        if self.path.as_ref().is_some_and(|path| path.is_empty()) {
            return Err("path cannot be empty".into());
        }
        if self
            .command_glob
            .as_ref()
            .is_some_and(|pattern| pattern.contains(['[', ']', '\\']))
        {
            return Err("command_glob supports only * and ? wildcards".into());
        }
        let selectors = [&self.command, &self.command_prefix, &self.command_glob];
        if selectors.iter().filter(|value| value.is_some()).count() > 1 {
            return Err("choose only one of command, command_prefix, command_glob".into());
        }
        if selectors
            .iter()
            .any(|value| value.as_ref().is_some_and(|s| s.trim().is_empty()))
        {
            return Err("command selectors cannot be empty".into());
        }
        if let Some(prefix) = &self.command_prefix
            && (!simple_prefix(prefix) || prefix.trim() != prefix)
        {
            return Err("command_prefix must contain simple literal command words".into());
        }
        if self.path.as_ref().is_some_and(|path| {
            Path::new(path).is_absolute() || path.split('/').any(|part| part == "..")
        }) {
            return Err("path must be workspace-relative and cannot contain ..".into());
        }
        Ok(())
    }

    fn matches(&self, tool: &str, args: &Value, root: &Path) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        if let Some(pattern) = &self.path {
            let Some(path) = args.get("path").and_then(Value::as_str) else {
                return false;
            };
            let Some(path) = relative_path(root, path) else {
                return false;
            };
            if !wildcard_match(pattern, &path) {
                return false;
            }
        }
        if self.command.is_none() && self.command_prefix.is_none() && self.command_glob.is_none() {
            return true;
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return false;
        };
        let raw_match = self.command.as_ref().is_some_and(|text| command == text)
            || self
                .command_prefix
                .as_ref()
                .is_some_and(|prefix| prefix_matches(prefix, command))
            || self
                .command_glob
                .as_ref()
                .is_some_and(|pattern| wildcard_match(pattern, command));
        if self.decision != RuleDecision::Allow {
            // Restrictive shell rules also match a denied subcommand in a chain.
            return raw_match
                || self.command_patterns().iter().any(|pattern| {
                    command_permission(command, &[], std::slice::from_ref(pattern))
                        == ShellPermission::Deny
                });
        }
        if !raw_match {
            return false;
        }
        // Every decomposed command must match. An opaque construct always asks.
        command_permission(command, &self.command_patterns(), &[]) == ShellPermission::Allow
    }

    fn command_patterns(&self) -> Vec<String> {
        if let Some(command) = &self.command {
            vec![command.clone()]
        } else if let Some(prefix) = &self.command_prefix {
            vec![prefix.clone(), format!("{prefix} *")]
        } else {
            self.command_glob.iter().cloned().collect()
        }
    }

    fn describe(&self) -> String {
        format!(
            "{:?} tool={} command={:?} prefix={:?} glob={:?} path={:?} — {}",
            self.decision,
            self.tool,
            self.command,
            self.command_prefix,
            self.command_glob,
            self.path,
            self.source.display()
        )
    }
}

pub(super) fn simple_prefix(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" _-./:=+".contains(&byte))
}

pub(super) fn prefix_matches(prefix: &str, command: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with(' '))
}

/// Delegate shell parsing and opaque-syntax decisions to the executor's public
/// policy API. This matcher never starts processes; its executor is used only for policy checks.
pub(super) fn command_permission(
    command: &str,
    allow: &[String],
    deny: &[String],
) -> ShellPermission {
    ShellExecutor::new(
        ShellConfig::new()
            .with_allowed_commands(allow.to_vec())
            .with_denied_commands(deny.to_vec()),
    )
    .check_permission(command)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    glob::Pattern::new(pattern).is_ok_and(|pattern| pattern.matches(text))
}

fn parse_rules(text: &str) -> Result<Vec<PermissionRule>, String> {
    let document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|err| err.to_string())?;
    if document.iter().any(|(key, _)| key != "rules") {
        return Err("unknown permission-file key; expected [[rules]]".into());
    }
    let Some(item) = document.get("rules") else {
        return Ok(Vec::new());
    };
    let tables = item
        .as_array_of_tables()
        .ok_or("rules must be an array of tables")?;
    tables
        .iter()
        .map(|table| {
            const KEYS: &[&str] = &[
                "tool",
                "decision",
                "command",
                "command_prefix",
                "command_glob",
                "path",
            ];
            if table.iter().any(|(key, _)| !KEYS.contains(&key)) {
                return Err("unknown rule key".into());
            }
            let string = |name: &str| -> Result<Option<String>, String> {
                table
                    .get(name)
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .ok_or_else(|| format!("{name} must be a string"))
                    })
                    .transpose()
            };
            let tool = string("tool")?.ok_or("rule requires tool")?;
            let decision = match string("decision")?.as_deref() {
                Some("allow") => RuleDecision::Allow,
                Some("ask") => RuleDecision::Ask,
                Some("deny") => RuleDecision::Deny,
                _ => return Err("decision must be allow, ask or deny".into()),
            };
            Ok(PermissionRule {
                tool,
                decision,
                command: string("command")?,
                command_prefix: string("command_prefix")?,
                command_glob: string("command_glob")?,
                path: string("path")?,
                source: PathBuf::new(),
                project: false,
            })
        })
        .collect()
}

fn relative_path(root: &Path, text: &str) -> Option<String> {
    let path = root.join(text);
    // Canonicalize an existing ancestor, including the parent of a new file.
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        suffix.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
    let mut canonical = ancestor.canonicalize().ok()?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    let relative = canonical.strip_prefix(root).ok()?;
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(relative.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::permission_rules::{PermissionCall, SessionGrants};
    use crate::agents::{PermissionMode, UnattendedGrants};
    use polaris::tools::ToolPermission;
    use serde_json::json;

    fn files(user: &str, project: &str) -> (tempfile::TempDir, PermissionRules) {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_path = dir.path().join("polaris.permissions.toml");
        std::fs::write(&user_path, user).unwrap();
        std::fs::write(&project_path, project).unwrap();
        let rules = PermissionRules::from_paths(dir.path(), Some(&user_path), Some(&project_path));
        (dir, rules)
    }

    #[test]
    fn deny_ask_allow_precedence_and_sources_cross_file_boundaries() {
        let (dir, rules) = files(
            r#"
[[rules]]
tool = "run_command"
decision = "allow"
command_prefix = "cargo test"
[[rules]]
tool = "run_command"
decision = "ask"
command_glob = "cargo test *secret*"
"#,
            r#"
[[rules]]
tool = "run_command"
decision = "deny"
command_glob = "cargo test *forbidden*"
"#,
        );
        assert!(
            rules.snapshot.errors.is_empty(),
            "{:?}",
            rules.snapshot.errors
        );
        for (command, decision, source) in [
            ("cargo test --lib", RuleDecision::Allow, "user.toml"),
            ("cargo test secret", RuleDecision::Ask, "user.toml"),
            (
                "cargo test secret-forbidden",
                RuleDecision::Deny,
                "polaris.permissions.toml",
            ),
            (
                "echo ok; cargo test forbidden",
                RuleDecision::Deny,
                "polaris.permissions.toml",
            ),
        ] {
            let found = rules
                .resolve("run_command", &json!({"command":command}))
                .unwrap();
            assert_eq!(found.0, decision, "{command}");
            assert!(found.1.contains(source));
        }
        assert!(rules.describe().contains(&dir.path().display().to_string()));
    }

    #[test]
    fn project_authority_requires_explicit_trust_and_opaque_syntax_never_autoallows() {
        let (_, rules) = files(
            "",
            r#"
[[rules]]
tool = "run_command"
decision = "allow"
command_prefix = "cargo test"
"#,
        );
        let safe = json!({"command":"cargo test --lib"});
        assert_eq!(
            rules.resolve("run_command", &safe).unwrap().0,
            RuleDecision::Ask
        );
        rules.trust_project();
        assert_eq!(
            rules.resolve("run_command", &safe).unwrap().0,
            RuleDecision::Allow
        );
        for command in [
            "cargo testing",
            "cargo test; echo surprise",
            "cargo test $(pwd)",
            "cargo test `pwd`",
        ] {
            assert!(
                rules
                    .resolve("run_command", &json!({"command":command}))
                    .is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn canonical_path_rules_cover_aliases_and_new_files() {
        let (dir, rules) = files(
            r#"
[[rules]]
tool = "read_file"
decision = "deny"
path = ".env"
[[rules]]
tool = "create_file"
decision = "ask"
path = "private/*"
"#,
            "",
        );
        std::fs::write(dir.path().join(".env"), "fixture").unwrap();
        std::fs::create_dir(dir.path().join("private")).unwrap();
        assert_eq!(
            rules
                .resolve("read_file", &json!({"path":"./.env"}))
                .unwrap()
                .0,
            RuleDecision::Deny
        );
        assert_eq!(
            rules
                .resolve("create_file", &json!({"path":"private/new.txt"}))
                .unwrap()
                .0,
            RuleDecision::Ask
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("alias")).unwrap();
            assert_eq!(
                rules
                    .resolve("read_file", &json!({"path":"alias"}))
                    .unwrap()
                    .0,
                RuleDecision::Deny
            );
        }
    }

    #[test]
    fn malformed_unknown_and_incomplete_rules_fail_closed() {
        for text in [
            "not valid toml",
            "unknown = true",
            "rules = 3",
            "[[rules]]\ntool = 'x'",
            "[[rules]]\ntool = 'x'\ndecision = 'allow'\ncommand_prefix = 'echo; rm'",
            "[[rules]]\ntool='x'\ndecision='deny'\npath='../secret'",
        ] {
            let (_, rules) = files(text, "");
            assert!(!rules.snapshot.errors.is_empty(), "{text}");
            assert_eq!(
                rules
                    .resolve("read_file", &json!({"path":"safe"}))
                    .unwrap()
                    .0,
                RuleDecision::Deny
            );
        }
    }

    #[test]
    fn each_malformed_rule_is_refused_with_its_own_reason() {
        for (rule, reason) in [
            ("tool = ' '\ndecision = 'deny'", "rule tool cannot be empty"),
            (
                "tool = 'x'\ndecision = 'deny'\npath = '['",
                "invalid wildcard pattern",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\ncommand_glob = '['",
                "invalid wildcard pattern",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\npath = ''",
                "path cannot be empty",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\ncommand_glob = 'a[bc]'",
                "command_glob supports only * and ? wildcards",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\ncommand = 'a'\ncommand_prefix = 'a'",
                "choose only one of command, command_prefix, command_glob",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\ncommand_prefix = 'a'\ncommand_glob = 'a*'",
                "choose only one of command, command_prefix, command_glob",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\ncommand = '  '",
                "command selectors cannot be empty",
            ),
            (
                "tool = 'x'\ndecision = 'allow'\ncommand_prefix = 'echo; rm'",
                "command_prefix must contain simple literal command words",
            ),
            (
                "tool = 'x'\ndecision = 'allow'\ncommand_prefix = ' echo'",
                "command_prefix must contain simple literal command words",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\npath = '/etc/passwd'",
                "path must be workspace-relative and cannot contain ..",
            ),
            (
                "tool = 'x'\ndecision = 'deny'\npath = 'a/../b'",
                "path must be workspace-relative and cannot contain ..",
            ),
        ] {
            let (dir, rules) = files(&format!("[[rules]]\n{rule}\n"), "");
            let snapshot = rules.snapshot();
            assert!(snapshot.rules.is_empty(), "{rule}");
            let [error] = snapshot.errors.as_slice() else {
                panic!("one error for {rule}: {:?}", snapshot.errors);
            };
            let expected = format!("{}: ", dir.path().join("user.toml").display());
            assert!(
                error.starts_with(&expected) && error.contains(reason),
                "{rule}: {error}"
            );
        }
        let (_, rules) = files("[[rules]]\ntool = 'x'\ndecision = 'deny'\n", "");
        assert!(rules.snapshot().errors.is_empty());
    }

    #[test]
    fn the_most_restrictive_decision_sorts_last() {
        assert!(RuleDecision::Allow < RuleDecision::Ask);
        assert!(RuleDecision::Ask < RuleDecision::Deny);
        // File order does not matter: the same two rules, written in either
        // order, resolve to the same decision.
        for order in [["allow", "ask"], ["ask", "allow"]] {
            let text: String = order
                .iter()
                .map(|decision| {
                    format!(
                        "[[rules]]\ntool = 'run_command'\ndecision = '{decision}'\ncommand_prefix = 'cargo test'\n"
                    )
                })
                .collect();
            let (_, rules) = files(&text, "");
            assert_eq!(
                rules
                    .resolve("run_command", &json!({"command":"cargo test"}))
                    .unwrap()
                    .0,
                RuleDecision::Ask,
                "{order:?}"
            );
        }
    }

    #[test]
    fn only_the_winning_rule_is_reported() {
        let (_, rules) = files(
            r#"
[[rules]]
tool = "run_command"
decision = "allow"
command_prefix = "cargo"
[[rules]]
tool = "run_command"
decision = "deny"
command_prefix = "cargo publish"
"#,
            "",
        );
        let (decision, source) = rules
            .resolve("run_command", &json!({"command":"cargo publish"}))
            .unwrap();
        assert_eq!(decision, RuleDecision::Deny);
        assert!(source.contains("cargo publish"), "{source}");
        assert!(!source.contains("Allow"), "{source}");
    }

    #[test]
    fn file_allows_never_widen_plan_or_unattended_grants_and_ask_refuses_unattended() {
        let (dir, rules) = files(
            r#"
[[rules]]
tool = "create_file"
decision = "allow"
[[rules]]
tool = "run_command"
decision = "ask"
command_prefix = "cargo test"
"#,
            "",
        );
        let grants = SessionGrants::default();
        let args = json!({"path":"new.txt"});
        let call = PermissionCall {
            tool: "create_file",
            args: &args,
            registry_default: ToolPermission::Confirm,
            root: dir.path(),
            generation: 1,
            ceiling: None,
        };
        for mode in [
            PermissionMode::Plan,
            PermissionMode::Unattended(UnattendedGrants::default()),
        ] {
            let mut policy = mode.permission_policy();
            policy.rules = Some(rules.clone());
            assert_eq!(
                policy.resolve_call(&call, &grants).permission,
                ToolPermission::Deny
            );
        }
        let mut policy = PermissionMode::Confirm.permission_policy();
        policy.rules = Some(rules.clone());
        assert_eq!(
            policy.resolve_call(&call, &grants).permission,
            ToolPermission::Allow
        );
        let mut policy = PermissionMode::Unattended(UnattendedGrants {
            edit_file: false,
            run_command_patterns: vec!["cargo *".into()],
        })
        .permission_policy();
        policy.rules = Some(rules);
        let args = json!({"command":"cargo test"});
        let call = PermissionCall {
            tool: "run_command",
            args: &args,
            registry_default: ToolPermission::Allow,
            root: dir.path(),
            generation: 1,
            ceiling: None,
        };
        assert_eq!(
            policy.resolve_call(&call, &grants).permission,
            ToolPermission::Deny
        );
    }

    const ALLOW_CARGO_TEST: &str = r#"
[[rules]]
tool = "run_command"
decision = "allow"
command_prefix = "cargo test"
"#;

    fn cargo_test_decision(rules: &PermissionRules) -> RuleDecision {
        rules
            .resolve("run_command", &json!({"command":"cargo test --lib"}))
            .unwrap()
            .0
    }

    /// A rules handle for a new session in the same project, as startup builds it.
    fn new_session(dir: &Path, store: &Path) -> PermissionRules {
        PermissionRules::from_paths(dir, None, Some(&dir.join("polaris.permissions.toml")))
            .with_project_trust(ProjectTrust::new(Some(store.to_path_buf()), dir))
    }

    #[test]
    fn trust_saved_for_the_rule_file_applies_to_later_sessions_until_it_changes() {
        let (dir, _) = files("", ALLOW_CARGO_TEST);
        let store = dir.path().join("trust").join("project-trust.json");
        let first = new_session(dir.path(), &store);
        assert_eq!(cargo_test_decision(&first), RuleDecision::Ask);
        assert_eq!(first.revision(), 1);
        first.trust_project();
        assert_eq!(first.revision(), 2);
        assert_eq!(
            first.remember_project_trust().unwrap(),
            [Subject::PermissionRules]
        );
        // Nothing but the saved decision differs between these two sessions.
        let second = new_session(dir.path(), &store);
        assert_eq!(cargo_test_decision(&second), RuleDecision::Allow);
        // Another project root with identical text is not trusted.
        let other = tempfile::tempdir().unwrap();
        std::fs::copy(
            dir.path().join("polaris.permissions.toml"),
            other.path().join("polaris.permissions.toml"),
        )
        .unwrap();
        assert_eq!(
            cargo_test_decision(&new_session(other.path(), &store)),
            RuleDecision::Ask
        );
        // An edit, even a harmless one, withdraws it until the user trusts the new text.
        std::fs::write(
            dir.path().join("polaris.permissions.toml"),
            format!("{ALLOW_CARGO_TEST}\n# edited\n"),
        )
        .unwrap();
        let edited = new_session(dir.path(), &store);
        assert_eq!(cargo_test_decision(&edited), RuleDecision::Ask);
        edited.remember_project_trust().unwrap();
        assert_eq!(
            cargo_test_decision(&new_session(dir.path(), &store)),
            RuleDecision::Allow
        );
    }

    #[test]
    fn remembering_without_a_store_fails_but_session_trust_still_works() {
        let (dir, rules) = files("", ALLOW_CARGO_TEST);
        let rules = rules.with_project_trust(ProjectTrust::new(None, dir.path()));
        assert!(matches!(
            rules.remember_project_trust(),
            Err(TrustError::NoStore)
        ));
        assert_eq!(cargo_test_decision(&rules), RuleDecision::Ask);
        rules.trust_project();
        assert_eq!(cargo_test_decision(&rules), RuleDecision::Allow);
    }

    #[test]
    fn remembering_with_no_project_rule_file_saves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("project-trust.json");
        let rules = PermissionRules::from_paths(
            dir.path(),
            None,
            Some(&dir.path().join("polaris.permissions.toml")),
        )
        .with_project_trust(ProjectTrust::new(Some(store.clone()), dir.path()));
        assert!(rules.remember_project_trust().unwrap().is_empty());
        assert!(!store.exists());
    }

    #[test]
    fn oversized_rule_files_fail_closed_without_being_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("polaris.permissions.toml");
        let mut text = String::from(ALLOW_CARGO_TEST);
        text.push('#');
        text.push_str(&"x".repeat(usize::try_from(MAX_FILE_BYTES).unwrap()));
        std::fs::write(&project, text).unwrap();
        let rules = PermissionRules::from_paths(dir.path(), None, Some(&project));
        let snapshot = rules.snapshot();
        assert_eq!(snapshot.errors.len(), 1, "{:?}", snapshot.errors);
        assert!(snapshot.errors[0].contains("larger than"));
        assert!(snapshot.rules.is_empty());
        assert_eq!(
            rules
                .resolve("read_file", &json!({"path":"src/lib.rs"}))
                .unwrap()
                .0,
            RuleDecision::Deny
        );
    }

    #[cfg(unix)]
    #[test]
    fn special_rule_files_fail_closed_without_being_read() {
        let dir = tempfile::tempdir().unwrap();
        // Reading a device would never finish.
        let rules = PermissionRules::from_paths(dir.path(), Some(Path::new("/dev/zero")), None);
        assert!(rules.snapshot().errors[0].contains("not a regular file"));
        let link = dir.path().join("polaris.permissions.toml");
        std::os::unix::fs::symlink("/dev/zero", &link).unwrap();
        let rules = PermissionRules::from_paths(dir.path(), None, Some(&link));
        assert!(rules.snapshot().errors[0].contains("not a regular file"));
        assert_eq!(
            rules.resolve("read_file", &json!({"path":"a"})).unwrap().0,
            RuleDecision::Deny
        );
        // A directory is not a rule file either.
        let rules = PermissionRules::from_paths(dir.path(), Some(dir.path()), None);
        assert!(rules.snapshot().errors[0].contains("not a regular file"));
    }
}
