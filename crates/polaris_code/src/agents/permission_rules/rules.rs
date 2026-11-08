use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use polaris::shell::{ShellConfig, ShellExecutor, ShellPermission};
use polaris::system::plugin::{Contract, Version};
use polaris::system::resource::GlobalResource;
use polaris_cosmos::contracts::ContentDigest;
use serde_json::Value;

use crate::project_trust::{MAX_FILE_BYTES, ProjectTrust, Subject, TrustError, read_content};
use crate::tools::command_check::command_parts;

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

/// Why a rule written in code could not be added to [`PermissionRules`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeRuleError {
    /// The name is empty, has leading or trailing whitespace, or contains a
    /// control character. Names are listed by `/permissions` and quoted in
    /// refusals, so they must fit on one line.
    InvalidName(String),
    /// A rule with this name was already added.
    Duplicate(String),
    /// The server is ready, so the rule set is fixed.
    Sealed(String),
}

impl fmt::Display for CodeRuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(name) => write!(
                f,
                "invalid code rule name {name:?}; use a non-empty single line without \
                 leading or trailing whitespace"
            ),
            Self::Duplicate(name) => write!(f, "a code rule named {name:?} already exists"),
            Self::Sealed(name) => write!(
                f,
                "cannot add code rule {name:?}: rules are fixed once the server is ready"
            ),
        }
    }
}

impl std::error::Error for CodeRuleError {}

/// What a rule written in code returns: no opinion, or a decision with the reason
/// the model and the operator are shown.
type CodeVerdict = Option<(RuleDecision, String)>;

type CodeRuleFn = dyn Fn(&str, &Value, &Path) -> CodeVerdict + Send + Sync;

/// One named rule written in code.
struct CodeRule {
    name: String,
    check: Box<CodeRuleFn>,
}

/// The rules plugins added in code, shared by every clone of one
/// [`PermissionRules`].
///
/// Sharing lets a clone taken early, such as the one behind `/permissions`, see
/// a rule a later plugin adds. Sealing at startup keeps the set fixed afterwards.
#[derive(Default)]
struct CodeRules {
    rules: RwLock<Vec<Arc<CodeRule>>>,
    sealed: AtomicBool,
}

impl fmt::Debug for CodeRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeRules")
            .field("names", &self.names())
            .field("sealed", &self.sealed.load(Ordering::SeqCst))
            .finish()
    }
}

impl CodeRules {
    fn add(&self, name: String, check: Box<CodeRuleFn>) -> Result<(), CodeRuleError> {
        if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
            return Err(CodeRuleError::InvalidName(name));
        }
        // The seal is checked under the write lock so a rule can never slip in
        // between sealing and the last read.
        let mut rules = self.rules.write().unwrap_or_else(PoisonError::into_inner);
        if self.sealed.load(Ordering::SeqCst) {
            return Err(CodeRuleError::Sealed(name));
        }
        if rules.iter().any(|rule| rule.name == name) {
            return Err(CodeRuleError::Duplicate(name));
        }
        rules.push(Arc::new(CodeRule { name, check }));
        Ok(())
    }

    fn seal(&self) {
        let _rules = self.rules.write().unwrap_or_else(PoisonError::into_inner);
        self.sealed.store(true, Ordering::SeqCst);
    }

    fn all(&self) -> Vec<Arc<CodeRule>> {
        self.rules
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn names(&self) -> Vec<String> {
        self.all().iter().map(|rule| rule.name.clone()).collect()
    }

    /// The most restrictive verdict among the rules, the earliest added winning
    /// a tie. `None` when no rule has an opinion.
    fn resolve(&self, root: &Path, tool: &str, args: &Value) -> CodeVerdict {
        let rules = self.all();
        if rules.is_empty() {
            return None;
        }
        let parts = ShellParts::of(tool, args);
        let mut winner: CodeVerdict = None;
        for rule in &rules {
            let Some((decision, reason)) = rule.evaluate(root, tool, args, &parts) else {
                continue;
            };
            if winner.as_ref().is_none_or(|(held, _)| decision > *held) {
                winner = Some((decision, reason));
            }
        }
        winner
    }
}

/// How a shell call splits for rules written in code.
enum ShellParts {
    /// Not a shell call, or one without command text: rules see the call once.
    Whole,
    /// A shell call whose command parsed into simple commands.
    Parsed(Vec<Value>),
    /// A shell call whose command did not parse into any simple command.
    Opaque,
}

impl ShellParts {
    /// Split a `run_command` or `run_in_background` call the way file rules see
    /// it: the whole command, then each simple command on its own.
    fn of(tool: &str, args: &Value) -> Self {
        if !matches!(tool, "run_command" | "run_in_background") {
            return Self::Whole;
        }
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return Self::Whole;
        };
        match command_parts(command) {
            Some(parts) if parts.is_empty() => Self::Opaque,
            Some(parts) if parts.len() == 1 && parts[0] == command => Self::Whole,
            Some(parts) => Self::Parsed(
                parts
                    .into_iter()
                    .map(|part| {
                        let mut part_args = args.clone();
                        part_args["command"] = Value::String(part);
                        part_args
                    })
                    .collect(),
            ),
            None => Self::Opaque,
        }
    }
}

impl CodeRule {
    /// Run the rule once, refusing the call if it panics.
    fn ask(&self, root: &Path, tool: &str, args: &Value) -> CodeVerdict {
        match catch_unwind(AssertUnwindSafe(|| (self.check)(tool, args, root))) {
            Ok(Some((decision, reason))) => Some((decision, self.reason(&reason))),
            Ok(None) => None,
            Err(_) => {
                tracing::error!(rule = %self.name, tool, "code permission rule panicked; refusing the call");
                Some((
                    RuleDecision::Deny,
                    format!(
                        "code rule {:?} failed while checking this call, so the call is refused",
                        self.name
                    ),
                ))
            }
        }
    }

    fn reason(&self, reason: &str) -> String {
        format!("code rule {:?}: {reason}", self.name)
    }

    /// This rule's verdict on the call.
    ///
    /// A shell call is put to the rule as a whole and then once per simple
    /// command, and the most restrictive answer stands. An allow stands only
    /// when the rule allowed the whole command and every part; for a command
    /// that did not parse an allow is dropped, as an opaque command never
    /// matches an allow in a rule file.
    fn evaluate(&self, root: &Path, tool: &str, args: &Value, parts: &ShellParts) -> CodeVerdict {
        let whole = self.ask(root, tool, args);
        let part_args = match parts {
            ShellParts::Whole => return whole,
            ShellParts::Opaque => {
                return whole.filter(|(decision, _)| *decision != RuleDecision::Allow);
            }
            ShellParts::Parsed(part_args) => part_args,
        };
        let verdicts: Vec<CodeVerdict> = std::iter::once(whole)
            .chain(part_args.iter().map(|part| self.ask(root, tool, part)))
            .collect();
        let mut restrictive: CodeVerdict = None;
        for (decision, reason) in verdicts.iter().flatten() {
            if *decision != RuleDecision::Allow
                && restrictive
                    .as_ref()
                    .is_none_or(|(held, _)| *decision > *held)
            {
                restrictive = Some((*decision, reason.clone()));
            }
        }
        if restrictive.is_some() {
            return restrictive;
        }
        if verdicts.iter().all(Option::is_some) {
            return verdicts.into_iter().next().flatten();
        }
        None
    }
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
/// Plugins can add named rules written in Rust while the server builds (see
/// [`add_code_rule`](Self::add_code_rule)). [`resolve`](Self::resolve) weighs
/// them together with the file rules: the most restrictive decision wins.
///
/// # Scope
///
/// Global: one set of rules governs every session of the process, and the
/// project-trust decision made in one session applies to all of them.
///
/// # Provided By
///
/// [`PermissionRulesPlugin`](super::PermissionRulesPlugin) loads the files and
/// inserts it as a mutable resource during `build`, so plugins that depend on it
/// can add code rules; it moves the resource to a global during `ready`, after
/// which the rules are fixed. The plugin's default holds no file rules.
///
/// # Access
///
/// Plugins that add code rules call [`add_code_rule`](Self::add_code_rule)
/// through `Server::get_resource_mut` during their own `build`. Systems read it
/// with `Res<PermissionRules>` only. The parsed snapshot is immutable and shared
/// by clones, and so are the code rules: a clone taken before a rule was added
/// sees it, and none can add one after startup. The one thing that can change
/// is the project-trust decision, held behind interior mutability and changed
/// only by the explicit `/permissions trust-project` command, so no system
/// mutates it through the resource.
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
    code: Arc<CodeRules>,
    project_trusted: Arc<AtomicBool>,
    project_digest: Option<ContentDigest>,
    trust: ProjectTrust,
}

impl GlobalResource for PermissionRules {}

impl Contract for PermissionRules {
    const CONTRACT_VERSION: Version = Version::new(0, 1, 0);
}

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
            code: Arc::default(),
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

    /// Add a rule written in code, named so `/permissions` and refusals can say
    /// which rule spoke.
    ///
    /// `rule` is called with the tool name, the call's arguments exactly as the
    /// model gave them, and the workspace root. It returns `None` for no opinion,
    /// or a [`RuleDecision`] and the reason to show the model and the operator.
    /// The decision is weighed like a rule from a permission file by
    /// [`resolve`](Self::resolve): the most restrictive decision wins, so a
    /// code `Deny` beats a file `Allow`, and a code `Allow` never beats a deny.
    /// Project trust does not apply to code rules, because they come from the
    /// assembly and not from the project folder. The permission mode, the
    /// ceiling and the unattended grants limit an `Allow` as they limit one in a
    /// file. In an unattended run an `Ask` is a refusal.
    ///
    /// A rule that panics refuses the call, with a reason naming the rule. A rule
    /// may be called several times for one call, so it should be a pure function
    /// of its inputs. For `run_command` and `run_in_background` it is called with
    /// the whole command and then with each simple command on its own; see
    /// [`PermissionRulesPlugin`](super::PermissionRulesPlugin) for details.
    ///
    /// Call this from a plugin's `build`, through `Server::get_resource_mut`, on a
    /// plugin that depends on [`PermissionRulesPlugin`](super::PermissionRulesPlugin).
    /// Clones share the rules.
    ///
    /// # Errors
    ///
    /// [`CodeRuleError::InvalidName`] for an empty, padded or multi-line name,
    /// [`CodeRuleError::Duplicate`] when the name is taken, and
    /// [`CodeRuleError::Sealed`] once the server is ready.
    pub fn add_code_rule(
        &mut self,
        name: impl Into<String>,
        rule: impl Fn(&str, &Value, &Path) -> Option<(RuleDecision, String)> + Send + Sync + 'static,
    ) -> Result<(), CodeRuleError> {
        self.code.add(name.into(), Box::new(rule))
    }

    /// Fix the code rules: later calls to `add_code_rule`, on this value or any
    /// clone, fail.
    pub(super) fn seal(&self) {
        self.code.seal();
    }

    /// Most restrictive rule matching this exact call, and its source text.
    ///
    /// Weighs the permission files and the rules plugins added in code together.
    /// Code rules are given the root the files were loaded for.
    #[must_use]
    pub fn resolve(&self, tool: &str, args: &Value) -> Option<(RuleDecision, String)> {
        self.resolve_in(&self.snapshot.root, tool, args)
    }

    /// [`resolve`](Self::resolve) with the workspace root of this call, which
    /// code rules receive. A session can work in a folder of its own, so the
    /// agent passes the call's root rather than the one the files were loaded
    /// for.
    pub(crate) fn resolve_in(
        &self,
        root: &Path,
        tool: &str,
        args: &Value,
    ) -> Option<(RuleDecision, String)> {
        if !self.snapshot.errors.is_empty() {
            return Some((RuleDecision::Deny, self.snapshot.errors.join("\n")));
        }
        let from_files = self
            .snapshot
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
            .map(|(decision, rule)| (decision, rule.describe()));
        // A file rule keeps the call when a code rule is no more restrictive.
        match (from_files, self.code.resolve(root, tool, args)) {
            (Some((held, _)), Some(code)) if code.0 > held => Some(code),
            (None, code) => code,
            (files, _) => files,
        }
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
        let code = self.code.names();
        if !code.is_empty() {
            lines.push("Code rules added by plugins:".into());
            lines.extend(code.iter().map(|name| format!("- {name:?}")));
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

    mod code_rules {
        use super::*;
        use crate::agents::PermissionCeiling;
        use crate::agents::permission_rules::ReviewEligibility;
        use std::sync::Mutex;

        type Verdict = Option<(RuleDecision, String)>;
        type Check = Box<dyn Fn(&str, &Value, &Path) -> Verdict + Send + Sync>;

        /// A rule that gives `decision` to calls whose `path` starts with `prefix`.
        fn on_path(prefix: &'static str, decision: RuleDecision) -> Check {
            Box::new(move |_, args, _| {
                let path = args.get("path")?.as_str()?;
                path.starts_with(prefix)
                    .then(|| (decision, format!("{prefix} is generated")))
            })
        }

        /// A rule that gives `decision` to calls whose `command` contains `needle`.
        fn on_command(needle: &'static str, decision: RuleDecision) -> Check {
            Box::new(move |_, args, _| {
                let command = args.get("command")?.as_str()?;
                command
                    .contains(needle)
                    .then(|| (decision, format!("{needle:?} is not allowed here")))
            })
        }

        /// A rule that gives `decision` to calls whose `command` starts with `prefix`.
        fn on_command_start(prefix: &'static str, decision: RuleDecision) -> Check {
            Box::new(move |_, args, _| {
                let command = args.get("command")?.as_str()?;
                command
                    .starts_with(prefix)
                    .then(|| (decision, "starts like this".to_string()))
            })
        }

        fn call<'a>(
            dir: &'a Path,
            tool: &'a str,
            args: &'a Value,
            registry_default: ToolPermission,
            ceiling: Option<&'a PermissionCeiling>,
        ) -> PermissionCall<'a> {
            PermissionCall {
                tool,
                args,
                registry_default,
                root: dir,
                generation: 1,
                ceiling,
            }
        }

        const FILE_ALLOWS_CREATE: &str = "[[rules]]\ntool = 'create_file'\ndecision = 'allow'\n";

        #[test]
        fn a_code_deny_beats_a_file_allow_and_names_the_rule() {
            let (dir, mut rules) = files(FILE_ALLOWS_CREATE, "");
            rules
                .add_code_rule(
                    "keep-out-of-generated",
                    on_path("generated/", RuleDecision::Deny),
                )
                .unwrap();
            let generated = json!({"path":"generated/a.rs"});
            assert_eq!(
                rules.resolve("create_file", &generated),
                Some((
                    RuleDecision::Deny,
                    r#"code rule "keep-out-of-generated": generated/ is generated"#.to_string()
                ))
            );
            // Other paths still follow the file.
            let (decision, source) = rules
                .resolve("create_file", &json!({"path":"src/a.rs"}))
                .unwrap();
            assert_eq!(decision, RuleDecision::Allow);
            assert!(source.contains("user.toml"), "{source}");

            // The agent's decision refuses the call and carries the same reason.
            let mut policy = PermissionMode::Confirm.permission_policy();
            policy.rules = Some(rules);
            let held = policy.resolve_call(
                &call(
                    dir.path(),
                    "create_file",
                    &generated,
                    ToolPermission::Confirm,
                    None,
                ),
                &SessionGrants::default(),
            );
            assert_eq!(held.permission, ToolPermission::Deny);
            assert_eq!(
                held.source,
                r#"code rule "keep-out-of-generated": generated/ is generated"#
            );
            assert!(!held.remember);
        }

        #[test]
        fn a_code_allow_never_beats_a_file_deny_or_ask() {
            for (file, held) in [("deny", RuleDecision::Deny), ("ask", RuleDecision::Ask)] {
                let (_, mut rules) = files(
                    &format!("[[rules]]\ntool = 'create_file'\ndecision = '{file}'\n"),
                    "",
                );
                rules
                    .add_code_rule("let-it-through", |_, _, _| {
                        Some((RuleDecision::Allow, "fine".into()))
                    })
                    .unwrap();
                let (decision, source) = rules
                    .resolve("create_file", &json!({"path":"a.rs"}))
                    .unwrap();
                assert_eq!(decision, held, "{file}");
                assert!(source.contains("user.toml"), "{source}");
            }
        }

        #[test]
        fn a_file_rule_keeps_a_tie_with_a_code_rule() {
            let (_, mut rules) = files("[[rules]]\ntool = 'create_file'\ndecision = 'deny'\n", "");
            rules
                .add_code_rule("also-no", on_path("", RuleDecision::Deny))
                .unwrap();
            let (decision, source) = rules
                .resolve("create_file", &json!({"path":"a.rs"}))
                .unwrap();
            assert_eq!(decision, RuleDecision::Deny);
            assert!(source.contains("user.toml"), "{source}");
        }

        #[test]
        fn among_code_rules_the_most_restrictive_wins_and_the_earliest_breaks_a_tie() {
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("asker", on_command("x", RuleDecision::Ask))
                .unwrap();
            rules
                .add_code_rule("allower", on_command("x", RuleDecision::Allow))
                .unwrap();
            let args = json!({"command":"x"});
            let (decision, reason) = rules.resolve("run_command", &args).unwrap();
            assert_eq!(decision, RuleDecision::Ask);
            assert!(reason.starts_with(r#"code rule "asker""#), "{reason}");

            rules
                .add_code_rule("denier", on_command("x", RuleDecision::Deny))
                .unwrap();
            rules
                .add_code_rule("second-denier", on_command("x", RuleDecision::Deny))
                .unwrap();
            let (decision, reason) = rules.resolve("run_command", &args).unwrap();
            assert_eq!(decision, RuleDecision::Deny);
            assert!(reason.starts_with(r#"code rule "denier""#), "{reason}");
            // No rule has an opinion about other calls.
            assert_eq!(rules.resolve("run_command", &json!({"command":"y"})), None);
        }

        #[test]
        fn a_code_allow_stays_inside_the_mode_and_the_ceiling() {
            let dir = tempfile::tempdir().unwrap();
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("trusted-edit", |tool, _, _| {
                    (tool == "create_file").then(|| (RuleDecision::Allow, "fine".to_string()))
                })
                .unwrap();
            let args = json!({"path":"new.txt"});
            let grants = SessionGrants::default();
            let decide = |mode: PermissionMode, ceiling: Option<&PermissionCeiling>| {
                let mut policy = mode.permission_policy();
                policy.rules = Some(rules.clone());
                policy.resolve_call(
                    &call(
                        dir.path(),
                        "create_file",
                        &args,
                        ToolPermission::Confirm,
                        ceiling,
                    ),
                    &grants,
                )
            };

            // Plan and unattended runs do not grow an edit grant.
            for mode in [
                PermissionMode::Plan,
                PermissionMode::Unattended(UnattendedGrants::default()),
            ] {
                assert_eq!(decide(mode, None).permission, ToolPermission::Deny);
            }
            // Where a person is asked by default, the allow removes the prompt.
            let allowed = decide(PermissionMode::Confirm, None);
            assert_eq!(allowed.permission, ToolPermission::Allow);
            assert!(
                allowed.source.contains("trusted-edit"),
                "{}",
                allowed.source
            );

            // The ceiling is applied last.
            let confirm = PermissionCeiling::new(PermissionMode::Confirm).unwrap();
            let bounded = decide(PermissionMode::AutoEdit, Some(&confirm));
            assert_eq!(bounded.permission, ToolPermission::Confirm);
            assert_eq!(bounded.source, "confirm permission ceiling");
            let plan = PermissionCeiling::new(PermissionMode::Plan).unwrap();
            let bounded = decide(PermissionMode::Confirm, Some(&plan));
            assert_eq!(bounded.permission, ToolPermission::Deny);
            assert_eq!(bounded.source, "plan permission ceiling");
        }

        #[test]
        fn a_code_ask_is_refused_when_unattended_and_left_to_a_person_otherwise() {
            let dir = tempfile::tempdir().unwrap();
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("review-tests", on_command("cargo test", RuleDecision::Ask))
                .unwrap();
            let args = json!({"command":"cargo test"});
            let grants = SessionGrants::default();

            let mut unattended = PermissionMode::Unattended(UnattendedGrants {
                edit_file: false,
                run_command_patterns: vec!["cargo *".into()],
            })
            .permission_policy();
            unattended.rules = Some(rules.clone());
            let refused = unattended.resolve_call(
                &call(
                    dir.path(),
                    "run_command",
                    &args,
                    ToolPermission::Allow,
                    None,
                ),
                &grants,
            );
            assert_eq!(refused.permission, ToolPermission::Deny);
            assert!(
                refused.source.contains("review-tests"),
                "{}",
                refused.source
            );

            let mut attended = PermissionMode::AutoEdit.permission_policy();
            attended.rules = Some(rules);
            let asked = attended.resolve_call(
                &call(
                    dir.path(),
                    "run_command",
                    &args,
                    ToolPermission::Allow,
                    None,
                ),
                &grants,
            );
            assert_eq!(asked.permission, ToolPermission::Confirm);
            assert_eq!(asked.review, ReviewEligibility::HumanOnly);
            assert!(!asked.remember);
        }

        #[test]
        fn project_trust_does_not_apply_to_code_rules() {
            let (_, mut rules) = files("", "[[rules]]\ntool = 'create_file'\ndecision = 'allow'\n");
            rules
                .add_code_rule("allow-edits", |tool, _, _| {
                    (tool == "edit_file").then(|| (RuleDecision::Allow, "fine".to_string()))
                })
                .unwrap();
            let create = json!({"path":"a.rs"});
            let edit = json!({"path":"a.rs", "old_text":"a", "new_text":"b"});
            // The project's allow asks until the project is trusted; the code
            // rule's allow does not wait for that.
            assert_eq!(
                rules.resolve("create_file", &create).unwrap().0,
                RuleDecision::Ask
            );
            assert_eq!(
                rules.resolve("edit_file", &edit).unwrap().0,
                RuleDecision::Allow
            );
            rules.trust_project();
            assert_eq!(
                rules.resolve("create_file", &create).unwrap().0,
                RuleDecision::Allow
            );
            assert_eq!(
                rules.resolve("edit_file", &edit).unwrap().0,
                RuleDecision::Allow
            );
        }

        #[test]
        fn a_rule_that_panics_refuses_the_call_and_names_itself() {
            let (_, mut rules) = files(FILE_ALLOWS_CREATE, "");
            rules
                .add_code_rule("crashy", |_, _, _| -> Verdict {
                    panic!("secret internals")
                })
                .unwrap();
            // Another rule that would allow does not rescue the call.
            rules
                .add_code_rule("allower", |_, _, _| {
                    Some((RuleDecision::Allow, "fine".into()))
                })
                .unwrap();
            let (decision, reason) = rules
                .resolve("create_file", &json!({"path":"a.rs"}))
                .unwrap();
            assert_eq!(decision, RuleDecision::Deny);
            assert_eq!(
                reason,
                r#"code rule "crashy" failed while checking this call, so the call is refused"#
            );
            assert!(!reason.contains("secret internals"), "{reason}");
        }

        #[test]
        fn a_rule_that_panics_on_one_part_of_a_command_refuses_the_whole_call() {
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("crashy", |_, args, _| -> Verdict {
                    let command = args["command"].as_str().unwrap();
                    assert!(command != "ls", "cannot handle ls");
                    None
                })
                .unwrap();
            let (decision, reason) = rules
                .resolve("run_command", &json!({"command":"echo a; ls"}))
                .unwrap();
            assert_eq!(decision, RuleDecision::Deny);
            assert!(reason.contains(r#"code rule "crashy""#), "{reason}");
        }

        #[test]
        fn names_are_checked_and_the_set_is_fixed_once_sealed() {
            let mut rules = PermissionRules::default();
            for name in ["", " padded", "padded ", "two\nlines", "tab\there"] {
                assert_eq!(
                    rules.add_code_rule(name, on_path("", RuleDecision::Deny)),
                    Err(CodeRuleError::InvalidName(name.to_string())),
                    "{name:?}"
                );
            }
            rules
                .add_code_rule("one", on_path("", RuleDecision::Deny))
                .unwrap();
            assert_eq!(
                rules.add_code_rule("one", on_path("", RuleDecision::Ask)),
                Err(CodeRuleError::Duplicate("one".into()))
            );
            let mut clone = rules.clone();
            rules.seal();
            for adder in [&mut rules, &mut clone] {
                let error = adder
                    .add_code_rule("two", on_path("", RuleDecision::Deny))
                    .unwrap_err();
                assert_eq!(error, CodeRuleError::Sealed("two".into()));
                assert!(error.to_string().contains("fixed"), "{error}");
            }
            // The rule added before sealing still applies.
            assert_eq!(
                rules.resolve("any", &json!({"path":"a"})).unwrap().0,
                RuleDecision::Deny
            );
            assert!(!rules.describe().contains("\"two\""));
        }

        #[test]
        fn clones_share_the_rules_added_to_any_of_them() {
            let mut original = PermissionRules::default();
            let early_clone = original.clone();
            original
                .add_code_rule("late", on_path("generated/", RuleDecision::Deny))
                .unwrap();
            let args = json!({"path":"generated/x"});
            assert_eq!(
                early_clone.resolve("create_file", &args).unwrap().0,
                RuleDecision::Deny
            );
            let mut other = early_clone.clone();
            other
                .add_code_rule("from-clone", on_path("tmp/", RuleDecision::Ask))
                .unwrap();
            assert!(original.describe().contains("\"from-clone\""));
            assert_eq!(
                original
                    .resolve("create_file", &json!({"path":"tmp/x"}))
                    .unwrap()
                    .0,
                RuleDecision::Ask
            );
        }

        #[test]
        fn describe_lists_code_rules_by_name_and_is_unchanged_without_any() {
            let (_, mut rules) = files(FILE_ALLOWS_CREATE, "");
            let before = rules.describe();
            assert!(!before.contains("Code rules"), "{before}");
            rules
                .add_code_rule("keep-out", on_path("generated/", RuleDecision::Deny))
                .unwrap();
            rules
                .add_code_rule("review-tests", on_path("tests/", RuleDecision::Ask))
                .unwrap();
            let listing = rules.describe();
            assert!(listing.starts_with(&before), "{listing}");
            assert!(
                listing
                    .contains("Code rules added by plugins:\n- \"keep-out\"\n- \"review-tests\""),
                "{listing}"
            );
        }

        #[test]
        fn the_rule_gets_the_tool_the_arguments_and_the_root() {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut rules = PermissionRules::default();
            let record = seen.clone();
            rules
                .add_code_rule("recorder", move |tool, args, root| {
                    record.lock().unwrap().push((
                        tool.to_string(),
                        args.clone(),
                        root.to_path_buf(),
                    ));
                    None
                })
                .unwrap();
            let args = json!({"path":"a.rs", "extra":[1, 2]});
            assert_eq!(rules.resolve("read_file", &args), None);
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(rules.resolve_in(dir.path(), "read_file", &args), None);
            let seen = seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            assert_eq!(seen[0].0, "read_file");
            assert_eq!(seen[0].1, args);
            assert_eq!(seen[1].2, dir.path());
        }

        /// What a rule is shown for one call, in order.
        fn commands_seen(tool: &str, command: &str) -> Vec<String> {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut rules = PermissionRules::default();
            let record = seen.clone();
            rules
                .add_code_rule("recorder", move |_, args, _| {
                    record.lock().unwrap().push(args.clone());
                    None
                })
                .unwrap();
            let args = json!({"command":command, "timeout_secs": 5});
            assert_eq!(rules.resolve(tool, &args), None);
            let seen = seen.lock().unwrap();
            for shown in seen.iter() {
                // Everything but the command text is passed through unchanged.
                assert_eq!(shown["timeout_secs"], 5);
            }
            seen.iter()
                .map(|shown| shown["command"].as_str().unwrap().to_string())
                .collect()
        }

        #[test]
        fn a_shell_call_is_shown_whole_then_one_simple_command_at_a_time() {
            for tool in ["run_command", "run_in_background"] {
                assert_eq!(
                    commands_seen(tool, "echo one && ls -l | wc -l; rm -rf build"),
                    [
                        "echo one && ls -l | wc -l; rm -rf build",
                        "echo one",
                        "ls -l",
                        "wc -l",
                        "rm -rf build"
                    ],
                    "{tool}"
                );
                // A single plain command is shown once.
                assert_eq!(commands_seen(tool, "cargo test"), ["cargo test"], "{tool}");
            }
            // Other tools are never split, even with a command-like argument.
            assert_eq!(commands_seen("read_file", "a; b"), ["a; b"]);
            // The text of a here document is data, not a command.
            let heredoc = "cat > notes.txt <<'EOF'\nrm -rf /\nEOF\nls";
            let seen = commands_seen("run_command", heredoc);
            assert_eq!(seen[0], heredoc);
            assert!(
                seen[1..].iter().all(|part| !part.contains("rm -rf")),
                "{seen:?}"
            );
            // Grammar the parser does not follow is shown once, as written.
            assert_eq!(
                commands_seen("run_command", "echo $(rm -rf build)"),
                ["echo $(rm -rf build)"]
            );
        }

        #[test]
        fn a_deny_on_any_part_of_a_shell_call_refuses_it() {
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("no-rm", on_command_start("rm ", RuleDecision::Deny))
                .unwrap();
            for tool in ["run_command", "run_in_background"] {
                for command in [
                    "rm -rf build",
                    "echo ok; rm -rf build",
                    "echo ok && rm -rf build",
                    "false || rm -rf build",
                    "ls | rm -rf build",
                    "cat > f <<'EOF'\nbody\nEOF\nrm -rf build",
                ] {
                    let (decision, reason) = rules
                        .resolve(tool, &json!({"command":command}))
                        .unwrap_or_else(|| panic!("{tool}: {command:?} has no verdict"));
                    assert_eq!(decision, RuleDecision::Deny, "{tool}: {command:?}");
                    assert!(reason.starts_with(r#"code rule "no-rm""#), "{reason}");
                }
                for command in [
                    "echo ok; ls",
                    "echo rm -rf build",
                    "cat > f <<'EOF'\nrm -rf build\nEOF",
                ] {
                    assert_eq!(
                        rules.resolve(tool, &json!({"command":command})),
                        None,
                        "{tool}: {command:?}"
                    );
                }
            }
        }

        #[test]
        fn a_shell_allow_needs_the_whole_command_and_every_part_to_agree() {
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("cargo-only", |_, args, _| {
                    let command = args.get("command")?.as_str()?;
                    command
                        .starts_with("cargo ")
                        .then(|| (RuleDecision::Allow, "cargo is fine".to_string()))
                })
                .unwrap();
            let decide = |command: &str| {
                rules
                    .resolve("run_command", &json!({"command":command}))
                    .map(|(decision, _)| decision)
            };
            assert_eq!(decide("cargo test"), Some(RuleDecision::Allow));
            assert_eq!(
                decide("cargo test && cargo build"),
                Some(RuleDecision::Allow)
            );
            // One part without an allow keeps the rule from allowing the call.
            assert_eq!(decide("cargo test && make"), None);
            assert_eq!(decide("cargo test; ls"), None);
            // So does a command the parser does not follow.
            assert_eq!(decide("cargo test $(whoami)"), None);
            // A deny elsewhere still wins over the allowed parts.
            rules
                .add_code_rule("no-publish", on_command("publish", RuleDecision::Deny))
                .unwrap();
            assert_eq!(
                rules
                    .resolve(
                        "run_command",
                        &json!({"command":"cargo test && cargo publish"})
                    )
                    .unwrap()
                    .0,
                RuleDecision::Deny
            );
        }

        #[test]
        fn an_opaque_command_is_still_refused_by_a_deny_on_the_whole_text() {
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("no-rm", on_command("rm -rf", RuleDecision::Deny))
                .unwrap();
            let (decision, _) = rules
                .resolve("run_command", &json!({"command":"echo $(rm -rf build)"}))
                .unwrap();
            assert_eq!(decision, RuleDecision::Deny);
        }

        #[test]
        fn a_shell_part_is_checked_through_the_agent_decision() {
            let dir = tempfile::tempdir().unwrap();
            let mut rules = PermissionRules::default();
            rules
                .add_code_rule("no-rm", on_command_start("rm ", RuleDecision::Deny))
                .unwrap();
            let mut policy = PermissionMode::AutoEdit.permission_policy();
            policy.rules = Some(rules);
            let args = json!({"command":"echo ok && rm -rf build"});
            let held = policy.resolve_call(
                &call(
                    dir.path(),
                    "run_command",
                    &args,
                    ToolPermission::Allow,
                    None,
                ),
                &SessionGrants::default(),
            );
            assert_eq!(held.permission, ToolPermission::Deny);
            assert!(
                held.source.contains(r#"code rule "no-rm""#),
                "{}",
                held.source
            );
        }
    }
}
