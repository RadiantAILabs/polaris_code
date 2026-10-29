//! A `.gitignore` subset for the file-mention walk.
//!
//! Enough of the format to keep `target/`, `node_modules/` and build output
//! out of the popup without a new dependency: blank lines and `#` comments,
//! `!` negation, a trailing `/` for directory-only rules, a leading `/` or an
//! interior `/` to anchor a pattern to the ignore file's directory, `*`, `?`,
//! `[…]` classes, and `**`. Later rules override earlier ones, as in git.
//! Not supported: escapes of leading `!`/`#`, global excludes, and
//! `.git/info/exclude`. A directory ruled out is never descended into, so a
//! negation cannot re-include something beneath an ignored directory — the
//! same limitation git has.

use std::path::Path;

/// One parsed line of a `.gitignore`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    negated: bool,
    dir_only: bool,
    /// Matched against the whole path relative to the ignore file when
    /// anchored; against any trailing run of components otherwise.
    anchored: bool,
    /// Pattern components split on `/`; `**` is one component.
    components: Vec<String>,
}

/// The rules of one `.gitignore`, in file order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    rules: Vec<Rule>,
}

impl IgnoreRules {
    /// Parse the contents of a `.gitignore`.
    #[must_use]
    pub fn parse(contents: &str) -> Self {
        let rules = contents.lines().filter_map(parse_line).collect();
        Self { rules }
    }

    /// Load `dir/.gitignore`, or an empty set when there is none.
    #[must_use]
    pub fn load(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(".gitignore"))
            .map(|contents| Self::parse(&contents))
            .unwrap_or_default()
    }

    /// Whether there is nothing to check.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The last rule's verdict for `relative` (a path relative to the ignore
    /// file's directory, `/`-separated): `Some(true)` ignored, `Some(false)`
    /// re-included by a negation, `None` unmentioned.
    #[must_use]
    pub fn verdict(&self, relative: &str, is_dir: bool) -> Option<bool> {
        let components: Vec<&str> = relative.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            return None;
        }
        let mut verdict = None;
        for rule in &self.rules {
            if rule.matches(&components, is_dir) {
                verdict = Some(!rule.negated);
            }
        }
        verdict
    }
}

fn parse_line(line: &str) -> Option<Rule> {
    // Trailing spaces are insignificant unless escaped; escapes are not
    // supported here, so just trim them.
    let line = line.trim_end();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (negated, pattern) = match line.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    let (dir_only, pattern) = match pattern.strip_suffix('/') {
        Some(rest) => (true, rest),
        None => (false, pattern),
    };
    let (anchored, pattern) = match pattern.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (pattern.contains('/'), pattern),
    };
    if pattern.is_empty() {
        return None;
    }
    let mut components: Vec<String> = pattern
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    // A leading `**/` is the unanchored case spelled out.
    let anchored = if components.first().is_some_and(|c| c == "**") {
        components.remove(0);
        false
    } else {
        anchored
    };
    if components.is_empty() {
        return None;
    }
    Some(Rule {
        negated,
        dir_only,
        anchored,
        components,
    })
}

impl Rule {
    /// Whether this rule matches `path` — either exactly, or as a directory
    /// prefix of it, since git ignores everything beneath a matched
    /// directory. A directory-only rule must land on a component that is a
    /// directory: any proper prefix of the path is one, and the whole path
    /// is when `is_dir`.
    fn matches(&self, path: &[&str], is_dir: bool) -> bool {
        let mut starts = if self.anchored { 0..1 } else { 0..path.len() };
        starts.any(|start| {
            let tail = &path[start..];
            (1..=tail.len())
                .filter(|&len| !self.dir_only || len < tail.len() || is_dir)
                .any(|len| match_exact(&self.components, &tail[..len]))
        })
    }
}

/// Match pattern components against path components, consuming both fully.
fn match_exact(pattern: &[String], path: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            // Zero or more components.
            (0..=path.len()).any(|skip| match_exact(rest, &path[skip..]))
        }
        Some((first, rest)) => match path.split_first() {
            Some((head, tail)) if glob_matches(first, head) => match_exact(rest, tail),
            _ => false,
        },
    }
}

/// Match one component against a glob: `*` any run, `?` one char, `[…]` a
/// class (`!` or `^` negates, `a-z` ranges), everything else literal.
fn glob_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_at(&pattern, &text)
}

fn glob_at(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| glob_at(&pattern[1..], &text[skip..])),
        Some('?') => !text.is_empty() && glob_at(&pattern[1..], &text[1..]),
        Some('[') => {
            let Some(close) = pattern.iter().position(|c| *c == ']').filter(|i| *i > 1) else {
                // No closing bracket: literal `[`.
                return text.first() == Some(&'[') && glob_at(&pattern[1..], &text[1..]);
            };
            let Some(&head) = text.first() else {
                return false;
            };
            let class = &pattern[1..close];
            let (negated, class) = match class.first() {
                Some('!' | '^') => (true, &class[1..]),
                _ => (false, class),
            };
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == '-' {
                    if class[i] <= head && head <= class[i + 2] {
                        hit = true;
                    }
                    i += 3;
                } else {
                    if class[i] == head {
                        hit = true;
                    }
                    i += 1;
                }
            }
            hit != negated && glob_at(&pattern[close + 1..], &text[1..])
        }
        Some(&literal) => text.first() == Some(&literal) && glob_at(&pattern[1..], &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(src: &str) -> IgnoreRules {
        IgnoreRules::parse(src)
    }

    #[test]
    fn basenames_match_at_any_depth_and_anchors_only_at_root() {
        let r = rules("*.log\n/build\ndocs/drafts\n");
        assert_eq!(r.verdict("a.log", false), Some(true));
        assert_eq!(r.verdict("deep/er/b.log", false), Some(true));
        assert_eq!(r.verdict("build", true), Some(true));
        assert_eq!(r.verdict("build/out.o", false), Some(true));
        assert_eq!(r.verdict("sub/build", true), None, "anchored: not at root");
        assert_eq!(r.verdict("docs/drafts/x.md", false), Some(true));
        assert_eq!(
            r.verdict("other/docs/drafts", true),
            None,
            "interior slash anchors"
        );
        assert_eq!(r.verdict("src/main.rs", false), None);
    }

    #[test]
    fn directory_only_negation_and_ordering() {
        let r = rules("target/\n*.tmp\n!keep.tmp\n# comment\n\n");
        assert_eq!(r.verdict("target", true), Some(true));
        assert_eq!(
            r.verdict("target", false),
            None,
            "a file named target is not a dir"
        );
        assert_eq!(
            r.verdict("target/out.o", false),
            Some(true),
            "beneath an ignored dir"
        );
        assert_eq!(
            r.verdict("a/target/out.o", false),
            Some(true),
            "at any depth"
        );
        assert_eq!(r.verdict("x.tmp", false), Some(true));
        assert_eq!(r.verdict("keep.tmp", false), Some(false), "negated later");
        let r = rules("!keep.tmp\n*.tmp\n");
        assert_eq!(r.verdict("keep.tmp", false), Some(true), "last rule wins");
    }

    #[test]
    fn double_star_and_classes() {
        let r = rules(
            "**/generated\nassets/**/*.png\nsrc/**\n[Tt]humbs.db\nlog[0-9].txt\n!log[!5].txt\n",
        );
        assert_eq!(r.verdict("generated", true), Some(true));
        assert_eq!(r.verdict("a/b/generated", true), Some(true));
        assert_eq!(r.verdict("assets/x.png", false), Some(true));
        assert_eq!(r.verdict("assets/deep/er/x.png", false), Some(true));
        assert_eq!(r.verdict("assets/x.jpg", false), None);
        assert_eq!(r.verdict("src/anything/at/all.rs", false), Some(true));
        assert_eq!(r.verdict("Thumbs.db", false), Some(true));
        assert_eq!(r.verdict("thumbs.db", false), Some(true));
        assert_eq!(r.verdict("log5.txt", false), Some(true));
        assert_eq!(r.verdict("log3.txt", false), Some(false));
    }

    #[test]
    fn unicode_and_spaces_in_patterns() {
        let r = rules("メモ*.txt\nmy docs/\n");
        assert_eq!(r.verdict("メモ1.txt", false), Some(true));
        assert_eq!(r.verdict("my docs", true), Some(true));
        assert_eq!(r.verdict("my docs/x", false), Some(true));
    }

    #[test]
    fn loads_from_disk_and_defaults_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(IgnoreRules::load(dir.path()).is_empty());
        std::fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
        assert_eq!(
            IgnoreRules::load(dir.path()).verdict("target", true),
            Some(true)
        );
    }
}
