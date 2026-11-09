//! Reading one shell command line for what it would do to a protected test.
//!
//! The reader tokenises the line the way a POSIX shell does, closely enough to
//! see through quoting, redirects, here-documents, command substitution,
//! `sh -c`, wrappers such as `env` and `timeout`, and variables assigned
//! earlier on the same line. It then asks of every simple command found, run
//! or not: would this change, remove, move or skip a protected path?
//!
//! It runs before the command does and keeps nothing between calls. It reads
//! files on disk (to expand a glob, to read a script the command is about to
//! run) but never the history of earlier calls. Over-approximation is the
//! intended direction: every command in a function body or a branch is checked
//! as though it ran.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::code_scan::{
    Finding, looks_like_a_test_file, mentioned_path, reads_the_tests, scan_running_code,
    scan_written_code,
};
use super::encoded::{CODE_ENCODED, encoded_code, is_code_path, is_shell_decoder};
use super::files::{ProtectedSet, RUNNER_WORDS, is_shared_config_name};
use super::solution::new_findings_in;

/// How many substitutions and interpreted scripts deep the reader goes.
const MAX_DEPTH: u32 = 6;

/// The largest script file the reader opens.
const MAX_SCRIPT_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone)]
enum Seg {
    Lit(String),
    Var(String),
    Unknown,
}

#[derive(Debug, Clone, Default)]
struct Word {
    segs: Vec<Seg>,
    glob: bool,
    subs: Vec<String>,
}

impl Word {
    /// The word's text with known variables filled in, and whether anything in
    /// it stayed unknown.
    fn resolve(&self, vars: &HashMap<String, String>) -> (String, bool) {
        let mut out = String::new();
        let mut dynamic = !self.subs.is_empty();
        for seg in &self.segs {
            match seg {
                Seg::Lit(s) => out.push_str(s),
                Seg::Var(name) => match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => dynamic = true,
                },
                Seg::Unknown => dynamic = true,
            }
        }
        (out, dynamic)
    }
}

#[derive(Debug, Clone)]
enum Tok {
    Word(Word),
    /// `;` `&` `&&` `||` `|` `|&` `(` `)` or a newline.
    Sep(String),
    /// A redirect operator, with the digits that came before it.
    Redir(String),
    /// The n-th here-document body.
    Heredoc(usize),
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    toks: Vec<Tok>,
    word: Word,
    in_word: bool,
    pending: Vec<(usize, String, bool)>,
    bodies: Vec<String>,
}

impl Lexer {
    fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            pos: 0,
            toks: Vec::new(),
            word: Word::default(),
            in_word: false,
            pending: Vec::new(),
            bodies: Vec::new(),
        }
    }

    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn push_lit(&mut self, c: char) {
        self.in_word = true;
        if let Some(Seg::Lit(s)) = self.word.segs.last_mut() {
            s.push(c);
        } else {
            self.word.segs.push(Seg::Lit(c.to_string()));
        }
    }

    fn flush(&mut self) {
        if self.in_word {
            let word = std::mem::take(&mut self.word);
            self.toks.push(Tok::Word(word));
            self.in_word = false;
        }
    }

    /// The text of a balanced `$( … )`, with the cursor after it.
    fn read_balanced_parens(&mut self) -> String {
        let mut depth = 1_u32;
        let mut out = String::new();
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            out.push(c);
        }
        out
    }

    fn read_backticks(&mut self) -> String {
        let mut out = String::new();
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '`' => break,
                '\\' if self.peek(0).is_some() => {
                    out.push(self.chars[self.pos]);
                    self.pos += 1;
                }
                _ => out.push(c),
            }
        }
        out
    }

    /// What follows a `$`: a variable, a substitution, or nothing special.
    fn dollar(&mut self, quoted: bool) {
        match self.peek(0) {
            Some('(') if self.peek(1) == Some('(') => {
                // Arithmetic: `$(( … ))`. The value is unknown.
                self.pos += 2;
                let mut depth = 2_u32;
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    if c == '(' {
                        depth += 1;
                    } else if c == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                }
                self.in_word = true;
                self.word.segs.push(Seg::Unknown);
            }
            Some('(') => {
                self.pos += 1;
                let inner = self.read_balanced_parens();
                self.in_word = true;
                self.word.subs.push(inner);
                self.word.segs.push(Seg::Unknown);
            }
            Some('{') => {
                self.pos += 1;
                let mut name = String::new();
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                self.in_word = true;
                let plain =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                self.word
                    .segs
                    .push(if plain { Seg::Var(name) } else { Seg::Unknown });
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut name = String::new();
                while let Some(c) = self.peek(0) {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        name.push(c);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.in_word = true;
                self.word.segs.push(Seg::Var(name));
            }
            Some(c)
                if c.is_ascii_digit() || matches!(c, '@' | '*' | '#' | '?' | '$' | '!' | '-') =>
            {
                self.pos += 1;
                self.in_word = true;
                self.word.segs.push(Seg::Unknown);
            }
            Some('\'') if !quoted => {
                // `$'…'` ANSI-C quoting: read as a single-quoted literal.
                self.pos += 1;
                self.in_word = true;
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    match c {
                        '\'' => break,
                        '\\' => {
                            if let Some(n) = self.peek(0) {
                                self.pos += 1;
                                self.push_lit(match n {
                                    'n' => '\n',
                                    't' => '\t',
                                    other => other,
                                });
                            }
                        }
                        other => self.push_lit(other),
                    }
                }
            }
            _ => self.push_lit('$'),
        }
    }

    fn double_quoted(&mut self) {
        self.in_word = true;
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                '"' => return,
                '\\' => {
                    if let Some(n) = self.peek(0) {
                        self.pos += 1;
                        if matches!(n, '"' | '\\' | '$' | '`') {
                            self.push_lit(n);
                        } else if n != '\n' {
                            self.push_lit('\\');
                            self.push_lit(n);
                        }
                    }
                }
                '$' => self.dollar(true),
                '`' => {
                    let inner = self.read_backticks();
                    self.word.subs.push(inner);
                    self.word.segs.push(Seg::Unknown);
                }
                other => self.push_lit(other),
            }
        }
    }

    fn single_quoted(&mut self) {
        self.in_word = true;
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            if c == '\'' {
                return;
            }
            self.push_lit(c);
        }
    }

    fn read_heredoc_bodies(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        for (index, delimiter, strip_tabs) in pending {
            let mut body = String::new();
            loop {
                if self.pos >= self.chars.len() {
                    break;
                }
                let mut line = String::new();
                while let Some(c) = self.peek(0) {
                    self.pos += 1;
                    if c == '\n' {
                        break;
                    }
                    line.push(c);
                }
                let compare = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if compare == delimiter {
                    break;
                }
                body.push_str(&line);
                body.push('\n');
            }
            if self.bodies.len() <= index {
                self.bodies.resize(index + 1, String::new());
            }
            self.bodies[index] = body;
        }
    }

    fn redirect_operator(&mut self, first: char) -> String {
        let mut op = String::from(first);
        match first {
            '>' => {
                if let Some(next @ ('>' | '|' | '&')) = self.peek(0) {
                    op.push(next);
                    self.pos += 1;
                }
            }
            '<' => match self.peek(0) {
                Some('<') => {
                    op.push('<');
                    self.pos += 1;
                    match self.peek(0) {
                        Some('<') => {
                            op.push('<');
                            self.pos += 1;
                        }
                        Some('-') => {
                            op.push('-');
                            self.pos += 1;
                        }
                        _ => {}
                    }
                }
                Some(next @ ('&' | '>')) => {
                    op.push(next);
                    self.pos += 1;
                }
                _ => {}
            },
            _ => {}
        }
        op
    }

    /// The delimiter word after `<<`, read directly so the body can be
    /// collected when the line ends.
    fn heredoc_delimiter(&mut self) -> String {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.pos += 1;
        }
        let mut out = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = self.peek(0) {
            match (quote, c) {
                (None, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>') => break,
                (None, '\'' | '"') => {
                    quote = Some(c);
                    self.pos += 1;
                }
                (Some(q), c) if c == q => {
                    quote = None;
                    self.pos += 1;
                }
                (_, '\\') if quote.is_none() => {
                    self.pos += 1;
                    if let Some(n) = self.peek(0) {
                        out.push(n);
                        self.pos += 1;
                    }
                }
                _ => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        out
    }

    fn lex(mut self) -> (Vec<Tok>, Vec<String>) {
        while let Some(c) = self.peek(0) {
            self.pos += 1;
            match c {
                ' ' | '\t' => self.flush(),
                '\n' => {
                    self.flush();
                    self.toks.push(Tok::Sep("\n".into()));
                    if !self.pending.is_empty() {
                        self.read_heredoc_bodies();
                    }
                }
                '#' if !self.in_word => {
                    while let Some(n) = self.peek(0) {
                        if n == '\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                ';' | '(' | ')' => {
                    self.flush();
                    if c == ';' && self.peek(0) == Some(';') {
                        self.pos += 1;
                    }
                    self.toks.push(Tok::Sep(c.to_string()));
                }
                '&' => {
                    if self.peek(0) == Some('>') {
                        self.flush();
                        self.pos += 1;
                        let mut op = String::from("&>");
                        if self.peek(0) == Some('>') {
                            op.push('>');
                            self.pos += 1;
                        }
                        self.toks.push(Tok::Redir(op));
                    } else {
                        self.flush();
                        let op = if self.peek(0) == Some('&') {
                            self.pos += 1;
                            "&&"
                        } else {
                            "&"
                        };
                        self.toks.push(Tok::Sep(op.into()));
                    }
                }
                '|' => {
                    self.flush();
                    let op = match self.peek(0) {
                        Some('|') => {
                            self.pos += 1;
                            "||"
                        }
                        Some('&') => {
                            self.pos += 1;
                            "|&"
                        }
                        _ => "|",
                    };
                    self.toks.push(Tok::Sep(op.into()));
                }
                '<' | '>' => {
                    if self.peek(0) == Some('(') {
                        // Process substitution: the inside is a command.
                        self.pos += 1;
                        let inner = self.read_balanced_parens();
                        self.in_word = true;
                        self.word.subs.push(inner);
                        self.word.segs.push(Seg::Unknown);
                        continue;
                    }
                    // Digits just before the operator name a file descriptor.
                    let fd_digits = self.in_word
                        && self.word.subs.is_empty()
                        && matches!(self.word.segs.as_slice(), [Seg::Lit(s)] if s.chars().all(|d| d.is_ascii_digit()));
                    if fd_digits {
                        self.word = Word::default();
                        self.in_word = false;
                    } else {
                        self.flush();
                    }
                    let op = self.redirect_operator(c);
                    if op == "<<" || op == "<<-" {
                        let delimiter = self.heredoc_delimiter();
                        let index = self.pending.len() + self.bodies.len();
                        self.pending.push((index, delimiter, op == "<<-"));
                        self.toks.push(Tok::Redir(op));
                        self.toks.push(Tok::Heredoc(index));
                    } else {
                        self.toks.push(Tok::Redir(op));
                    }
                }
                '\'' => self.single_quoted(),
                '"' => self.double_quoted(),
                '\\' => match self.peek(0) {
                    Some('\n') => self.pos += 1,
                    Some(n) => {
                        self.pos += 1;
                        self.push_lit(n);
                    }
                    None => {}
                },
                '$' => self.dollar(false),
                '`' => {
                    let inner = self.read_backticks();
                    self.in_word = true;
                    self.word.subs.push(inner);
                    self.word.segs.push(Seg::Unknown);
                }
                '*' | '?' | '[' => {
                    self.word.glob = true;
                    self.push_lit(c);
                }
                other => self.push_lit(other),
            }
        }
        self.flush();
        // A here-document the input ended inside still has its body.
        if !self.pending.is_empty() {
            self.read_heredoc_bodies();
        }
        (self.toks, self.bodies)
    }
}

#[derive(Debug, Clone, Default)]
struct Simple {
    words: Vec<Word>,
    redirs: Vec<(String, Option<Word>, Option<usize>)>,
    piped_from_previous: bool,
    pipes_to_next: bool,
}

const RESERVED: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "in", "function", "select", "!", "{", "}", "[[", "]]",
];

fn build(toks: Vec<Tok>) -> Vec<Simple> {
    let mut out: Vec<Simple> = Vec::new();
    let mut cur = Simple::default();
    let mut carry_pipe = false;
    let mut iter = toks.into_iter().peekable();
    let finish =
        |cur: &mut Simple, out: &mut Vec<Simple>, pipes_to_next: bool, carry: &mut bool| {
            cur.pipes_to_next = pipes_to_next;
            if !cur.words.is_empty() || !cur.redirs.is_empty() {
                out.push(std::mem::take(cur));
            } else {
                *cur = Simple::default();
            }
            *carry = pipes_to_next;
        };
    while let Some(tok) = iter.next() {
        match tok {
            Tok::Word(word) => cur.words.push(word),
            Tok::Sep(sep) => {
                let pipe = sep == "|" || sep == "|&";
                finish(&mut cur, &mut out, pipe, &mut carry_pipe);
                cur.piped_from_previous = carry_pipe;
            }
            Tok::Redir(op) => match iter.next() {
                Some(Tok::Word(target)) => cur.redirs.push((op, Some(target), None)),
                Some(Tok::Heredoc(index)) => cur.redirs.push((op, None, Some(index))),
                Some(other) => {
                    // An operator with no target; keep the token for the next round.
                    cur.redirs.push((op, None, None));
                    if let Tok::Sep(sep) = other {
                        let pipe = sep == "|" || sep == "|&";
                        finish(&mut cur, &mut out, pipe, &mut carry_pipe);
                        cur.piped_from_previous = carry_pipe;
                    }
                }
                None => cur.redirs.push((op, None, None)),
            },
            Tok::Heredoc(_) => {}
        }
    }
    finish(&mut cur, &mut out, false, &mut carry_pipe);
    out
}

fn expand_braces(text: &str) -> Vec<String> {
    fn go(text: &str, depth: usize, out: &mut Vec<String>) {
        if out.len() >= 64 {
            return;
        }
        let Some(open) = text.find('{') else {
            out.push(text.to_string());
            return;
        };
        let bytes: Vec<char> = text.chars().collect();
        let start = text[..open].chars().count();
        let mut level = 0_i32;
        let mut close = None;
        let mut commas = Vec::new();
        for (i, c) in bytes.iter().enumerate().skip(start) {
            match c {
                '{' => level += 1,
                '}' => {
                    level -= 1;
                    if level == 0 {
                        close = Some(i);
                        break;
                    }
                }
                ',' if level == 1 => commas.push(i),
                _ => {}
            }
        }
        let Some(close) = close else {
            out.push(text.to_string());
            return;
        };
        if commas.is_empty() || depth > 4 {
            out.push(text.to_string());
            return;
        }
        let prefix: String = bytes[..start].iter().collect();
        let suffix: String = bytes[close + 1..].iter().collect();
        let mut from = start + 1;
        let mut parts = Vec::new();
        for comma in commas {
            parts.push(bytes[from..comma].iter().collect::<String>());
            from = comma + 1;
        }
        parts.push(bytes[from..close].iter().collect::<String>());
        for part in parts {
            go(&format!("{prefix}{part}{suffix}"), depth + 1, out);
        }
    }
    let mut out = Vec::new();
    go(text, 0, &mut out);
    out
}

fn basename(command: &str) -> &str {
    command.rsplit('/').next().unwrap_or(command)
}

/// One resolved word.
#[derive(Debug, Clone)]
struct Arg {
    text: String,
    dynamic: bool,
    glob: bool,
}

impl Arg {
    fn is_option(&self) -> bool {
        self.text.starts_with('-') && self.text.len() > 1 && !self.dynamic
    }
}

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "command", "exec", "nohup", "time", "timeout", "nice", "ionice",
    "setsid", "stdbuf", "unbuffer", "builtin", "xargs",
];

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "ash"];

const INTERPRETERS: &[&str] = &[
    "python", "python2", "python3", "pypy", "pypy3", "node", "nodejs", "deno", "bun", "perl",
    "ruby", "php", "lua", "awk", "gawk", "mawk", "Rscript", "julia", "tclsh",
];

const EDITORS: &[&str] = &["ed", "ex", "vi", "vim", "nvim", "nano", "emacs"];

/// Commands every non-option argument of which is a path they change.
const PATH_MUTATORS: &[&str] = &[
    "rm", "rmdir", "unlink", "shred", "mkdir", "touch", "truncate", "chmod", "chown", "chgrp",
    "chattr", "setfacl", "mount", "umount", "mkfifo", "mknod",
];

struct Reader<'a> {
    protected: &'a ProtectedSet,
    cwd: PathBuf,
    vars: HashMap<String, String>,
    written: HashMap<PathBuf, String>,
    depth: u32,
    found: Option<Finding>,
    /// Whether a file the command writes is judged as the moved setup judges a
    /// file an edit writes: on what it would hold afterwards, and only for what
    /// the write adds.
    moved: bool,
}

/// The first reason a protected test would be changed, removed, moved or
/// skipped by running `command` from `cwd`, or `None`.
#[must_use]
pub fn check_command(protected: &ProtectedSet, cwd: &Path, command: &str) -> Option<Finding> {
    read_command(protected, cwd, command, false)
}

/// [`check_command`] for the setup that keeps the rules out of the prompt: text
/// the command writes into a file of the work is read by the solution rules
/// ([`super::solution`]) against the file as it stands, not by the earlier
/// rule about values copied from the tests.
#[must_use]
pub fn check_command_moved(protected: &ProtectedSet, cwd: &Path, command: &str) -> Option<Finding> {
    read_command(protected, cwd, command, true)
}

fn read_command(
    protected: &ProtectedSet,
    cwd: &Path,
    command: &str,
    moved: bool,
) -> Option<Finding> {
    let mut reader = Reader {
        protected,
        cwd: cwd.to_path_buf(),
        vars: HashMap::new(),
        written: HashMap::new(),
        depth: 0,
        found: None,
        moved,
    };
    reader.run(command);
    reader.found
}

impl Reader<'_> {
    fn flag(&mut self, code: &'static str, reason: impl Into<String>) {
        if self.found.is_none() {
            self.found = Some(Finding::new(code, reason));
        }
    }

    /// What reading code a command is about to run finds. The setup that
    /// reads the work's files also looks for code kept in an encoded form.
    fn scan_code(&self, code: &str) -> Option<Finding> {
        scan_running_code(code, self.protected).or_else(|| {
            if !self.moved {
                return None;
            }
            encoded_code(code).map(|what| Finding::new(CODE_ENCODED, format!("this code {what}")))
        })
    }

    /// Whether a command decodes its input into a source file: `base64 -d`
    /// redirected to a `.py` file, or `xxd -r` given one to write.
    fn decodes_into_code(&self, simple: &Simple, name: &str, args: &[Arg]) -> bool {
        let texts: Vec<&str> = args.iter().map(|a| a.text.as_str()).collect();
        if !is_shell_decoder(name, &texts) {
            return false;
        }
        let writes_code = simple
            .redirs
            .iter()
            .filter(|(op, _, _)| matches!(op.as_str(), ">" | ">>" | ">|" | "&>" | "&>>" | "<>"))
            .filter_map(|(_, target, _)| target.as_ref())
            .map(|target| self.arg(target))
            .any(|target| !target.dynamic && is_code_path(&target.text));
        let names_code = matches!(name, "xxd" | "openssl")
            && args
                .iter()
                .any(|a| !a.is_option() && !a.dynamic && is_code_path(&a.text));
        writes_code || names_code
    }

    fn sub_reader(&self) -> Reader<'_> {
        Reader {
            protected: self.protected,
            cwd: self.cwd.clone(),
            vars: self.vars.clone(),
            written: self.written.clone(),
            depth: self.depth + 1,
            found: None,
            moved: self.moved,
        }
    }

    fn run_nested(&mut self, text: &str) {
        if self.depth >= MAX_DEPTH {
            self.flag(
                "too-deep",
                "this command nests shells or substitutions too deeply to be checked",
            );
            return;
        }
        let mut sub = self.sub_reader();
        sub.run(text);
        let found = sub.found.take();
        if let Some(f) = found {
            self.flag(f.code, f.reason);
        }
    }

    fn run(&mut self, text: &str) {
        let (toks, bodies) = Lexer::new(text).lex();
        let simples = build(toks);
        let mut previous_text: Option<String> = None;
        for simple in &simples {
            if self.found.is_some() {
                return;
            }
            let piped_text = if simple.piped_from_previous {
                previous_text.clone()
            } else {
                None
            };
            let literal = self.simple(
                simple,
                &bodies,
                piped_text.as_deref(),
                simple.piped_from_previous,
            );
            previous_text = literal;
        }
    }

    fn arg(&self, word: &Word) -> Arg {
        let (text, dynamic) = word.resolve(&self.vars);
        Arg {
            text,
            dynamic,
            glob: word.glob,
        }
    }

    /// Check one simple command. Returns the text it would print when that
    /// is known (for `echo` and `printf`), so a pipe into an interpreter can
    /// be read.
    fn simple(
        &mut self,
        simple: &Simple,
        bodies: &[String],
        piped_text: Option<&str>,
        piped: bool,
    ) -> Option<String> {
        // Command substitutions run first.
        let subs: Vec<String> = simple
            .words
            .iter()
            .flat_map(|w| w.subs.iter().cloned())
            .chain(
                simple
                    .redirs
                    .iter()
                    .filter_map(|(_, w, _)| w.as_ref())
                    .flat_map(|w| w.subs.iter().cloned()),
            )
            .collect();
        for sub in subs {
            self.run_nested(&sub);
            if self.found.is_some() {
                return None;
            }
        }

        let mut words: Vec<Arg> = simple.words.iter().map(|w| self.arg(w)).collect();

        // Leading `NAME=value` words.
        let mut assigns: Vec<(String, String, bool)> = Vec::new();
        while let Some(first) = words.first() {
            let Some((name, value)) = first.text.split_once('=') else {
                break;
            };
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || name.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                break;
            }
            assigns.push((name.to_string(), value.to_string(), first.dynamic));
            words.remove(0);
        }
        for (name, _, _) in &assigns {
            if matches!(name.as_str(), "PYTEST_ADDOPTS" | "PYTEST_PLUGINS") {
                self.flag(
                    "changes-test-run",
                    format!("this sets `{name}`, which changes which tests run and how"),
                );
                return None;
            }
        }
        if words.is_empty() {
            for (name, value, dynamic) in assigns {
                if dynamic {
                    self.vars.remove(&name);
                } else {
                    self.vars.insert(name, value);
                }
            }
            self.redirects(simple, bodies, &[], None);
            return None;
        }

        // Skip reserved words and unwrap wrappers.
        while let Some(first) = words.first() {
            if RESERVED.contains(&first.text.as_str()) {
                words.remove(0);
            } else {
                break;
            }
        }
        let mut via_xargs = false;
        while let Some(first) = words.first() {
            let name = basename(&first.text).to_string();
            if !WRAPPERS.contains(&name.as_str()) {
                break;
            }
            words.remove(0);
            if name == "xargs" {
                via_xargs = true;
            }
            while let Some(next) = words.first() {
                let looks_like_option = next.text.starts_with('-') && next.text.len() > 1;
                let is_assignment =
                    name == "env" && next.text.contains('=') && !next.text.starts_with('-');
                let is_duration = name == "timeout"
                    && next
                        .text
                        .trim_end_matches(['s', 'm', 'h', 'd'])
                        .parse::<f64>()
                        .is_ok();
                if looks_like_option || is_assignment || is_duration {
                    words.remove(0);
                } else {
                    break;
                }
            }
        }
        let Some(first) = words.first().cloned() else {
            self.redirects(simple, bodies, &[], None);
            return None;
        };
        if first.dynamic {
            self.flag(
                "opaque-command",
                "the command to run is built from a variable or substitution that cannot be read before it runs",
            );
            return None;
        }
        let name = basename(&first.text).to_string();
        let args: Vec<Arg> = words[1..].to_vec();

        // `cd` moves the directory later commands in this line start from.
        if matches!(name.as_str(), "cd" | "pushd") {
            if let Some(target) = args.iter().find(|a| !a.is_option())
                && !target.dynamic
            {
                self.cwd = self.protected.resolve(&self.cwd, &target.text);
            }
            return None;
        }
        if name == "export" || name == "declare" || name == "typeset" || name == "local" {
            for arg in &args {
                if let Some((n, v)) = arg.text.split_once('=') {
                    if matches!(n, "PYTEST_ADDOPTS" | "PYTEST_PLUGINS") {
                        self.flag(
                            "changes-test-run",
                            format!("this sets `{n}`, which changes which tests run and how"),
                        );
                        return None;
                    }
                    if arg.dynamic {
                        self.vars.remove(n);
                    } else {
                        self.vars.insert(n.to_string(), v.to_string());
                    }
                }
            }
            return None;
        }

        let heredoc = simple
            .redirs
            .iter()
            .find_map(|(op, _, index)| {
                (op.starts_with("<<") && op != "<<<")
                    .then_some(*index)
                    .flatten()
            })
            .and_then(|i| bodies.get(i).cloned());
        let stdin_text: Option<String> = heredoc.clone().or_else(|| piped_text.map(str::to_string));

        // The text this command prints, for a later pipe or a redirect.
        let printed = matches!(name.as_str(), "echo" | "printf" | "cat")
            .then(|| {
                if name == "cat" {
                    heredoc.clone()
                } else if args.iter().any(|a| a.dynamic) {
                    None
                } else {
                    Some(
                        args.iter()
                            .filter(|a| !a.is_option())
                            .map(|a| a.text.clone())
                            .collect::<Vec<_>>()
                            .join(" "),
                    )
                }
            })
            .flatten();

        if self.moved && self.decodes_into_code(simple, &name, &args) {
            self.flag(
                CODE_ENCODED,
                "this command decodes text into a source file, so the code in that file is not the code that can be read",
            );
            return None;
        }

        self.redirects(simple, bodies, &args, printed.as_deref());
        if self.found.is_some() {
            return None;
        }

        self.dispatch(&name, &args, stdin_text.as_deref(), piped, via_xargs);

        if matches!(name.as_str(), "echo" | "printf" | "cat") {
            printed
        } else {
            None
        }
    }

    fn redirects(
        &mut self,
        simple: &Simple,
        bodies: &[String],
        _args: &[Arg],
        printed: Option<&str>,
    ) {
        for (op, target, index) in &simple.redirs {
            let writes = matches!(op.as_str(), ">" | ">>" | ">|" | "&>" | "&>>" | "<>");
            if !writes {
                continue;
            }
            let Some(target) = target else {
                continue;
            };
            let arg = self.arg(target);
            if arg.text.starts_with("&") || arg.text == "-" {
                continue;
            }
            if arg.dynamic {
                self.flag(
                    "opaque-redirect",
                    "this command writes to a path built from a variable or substitution that cannot be read before it runs; write the path out",
                );
                return;
            }
            if arg.text.starts_with("/dev/") {
                continue;
            }
            let abs = self.protected.resolve(&self.cwd, &arg.text);
            if let Some(touch) = self.protected.touch(&abs) {
                self.flag(
                    "shell-redirect",
                    format!("this command writes to {}", touch.describe()),
                );
                return;
            }
            // Text written into a file in the tree is read like any other
            // code the call writes.
            let text = index
                .and_then(|i| bodies.get(i).cloned())
                .or_else(|| printed.map(str::to_string));
            if let Some(text) = text {
                let rel = self.protected.relative(&abs);
                let found = if self.moved {
                    let appends = matches!(op.as_str(), ">>" | "&>>");
                    self.moved_finding(&abs, &rel, &text, appends)
                } else {
                    scan_written_code(&text, &rel, self.protected)
                };
                if let Some(found) = found {
                    self.flag(found.code, found.reason);
                    return;
                }
                self.written.insert(abs, text);
            }
        }
    }

    /// What the moved setup finds in text a redirect writes to `abs`: whether the
    /// text reads the tests, and what the file would newly hold afterwards.
    fn moved_finding(&self, abs: &Path, rel: &str, text: &str, appends: bool) -> Option<Finding> {
        if !looks_like_a_test_file(rel)
            && let Some(found) = reads_the_tests(text, self.protected)
        {
            return Some(found);
        }
        let current = std::fs::read_to_string(abs).unwrap_or_default();
        let after = if appends {
            format!("{current}{text}")
        } else {
            text.to_string()
        };
        new_findings_in(self.protected, rel, &current, &after)
            .into_iter()
            .next()
    }

    /// Resolve a path argument and every place it could name: braces and
    /// globs expand against the tree.
    fn candidate_paths(&mut self, arg: &Arg) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for text in expand_braces(&arg.text) {
            if arg.glob {
                let pattern = if Path::new(&text).is_absolute() {
                    text.clone()
                } else {
                    self.cwd.join(&text).to_string_lossy().into_owned()
                };
                let options = glob::MatchOptions {
                    case_sensitive: true,
                    require_literal_separator: true,
                    require_literal_leading_dot: true,
                };
                if let Ok(paths) = glob::glob_with(&pattern, options) {
                    for path in paths.flatten().take(256) {
                        out.push(crate::rules::files::resolve_existing_prefix(&path));
                    }
                }
            } else {
                out.push(self.protected.resolve(&self.cwd, &text));
            }
        }
        out
    }

    fn check_paths(&mut self, command: &str, verb: &str, args: &[&Arg]) {
        for arg in args {
            if arg.dynamic {
                self.flag(
                    "opaque-path",
                    format!(
                        "`{command}` would {verb} a path built from a variable or substitution that cannot be read before it runs; write the path out"
                    ),
                );
                return;
            }
            for abs in self.candidate_paths(arg) {
                if let Some(touch) = self.protected.touch(&abs) {
                    self.flag(
                        "shell-mutates-protected-path",
                        format!("`{command}` would {verb} {}", touch.describe()),
                    );
                    return;
                }
            }
        }
    }

    fn dispatch(
        &mut self,
        name: &str,
        args: &[Arg],
        stdin_text: Option<&str>,
        piped: bool,
        via_xargs: bool,
    ) {
        let non_options: Vec<&Arg> = args.iter().filter(|a| !a.is_option()).collect();

        if via_xargs
            && (PATH_MUTATORS.contains(&name) || matches!(name, "mv" | "cp" | "tee" | "sed" | "ln"))
        {
            self.flag(
                "opaque-path",
                format!("`xargs {name}` would act on paths that cannot be read before it runs"),
            );
            return;
        }

        if PATH_MUTATORS.contains(&name) {
            let verb = match name {
                "rm" | "rmdir" | "unlink" | "shred" => "remove",
                "chmod" | "chown" | "chgrp" | "chattr" | "setfacl" => "change the permissions of",
                "truncate" => "truncate",
                "touch" => "change the times of",
                _ => "change",
            };
            self.check_paths(name, verb, &non_options);
            return;
        }

        match name {
            "mv" => self.check_paths(name, "move or replace", &non_options),
            "cp" | "install" | "rsync" => {
                let mut dest: Vec<&Arg> = Vec::new();
                let mut iter = args.iter();
                let mut explicit = false;
                while let Some(arg) = iter.next() {
                    if arg.text == "-t" || arg.text == "--target-directory" {
                        if let Some(d) = iter.next() {
                            dest.push(d);
                            explicit = true;
                        }
                    } else if let Some(d) = arg.text.strip_prefix("--target-directory=") {
                        let _ = d;
                        dest.push(arg);
                        explicit = true;
                    }
                }
                if !explicit && let Some(last) = non_options.last() {
                    dest.push(last);
                }
                self.check_paths(name, "overwrite", &dest);
            }
            "ln" => {
                if let Some(last) = non_options.last() {
                    self.check_paths(name, "replace", &[*last]);
                }
            }
            "tee" => self.check_paths(name, "write to", &non_options),
            "dd" => {
                let outputs: Vec<Arg> = args
                    .iter()
                    .filter_map(|a| {
                        a.text.strip_prefix("of=").map(|p| Arg {
                            text: p.to_string(),
                            dynamic: a.dynamic,
                            glob: false,
                        })
                    })
                    .collect();
                let refs: Vec<&Arg> = outputs.iter().collect();
                self.check_paths(name, "overwrite", &refs);
            }
            "sed" | "perl" | "ruby" | "awk" | "gawk" | "mawk"
                if args.iter().any(|a| in_place_flag(name, a, args)) =>
            {
                self.check_paths(name, "rewrite in place", &non_options);
                if self.found.is_none() && name != "sed" {
                    self.interpreter(name, args, stdin_text, piped);
                }
            }
            "patch" => self.patch(args, stdin_text),
            "find" => self.find(args),
            "git" => self.git(args, stdin_text),
            "curl" | "wget" => {
                let outputs: Vec<&Arg> = args
                    .windows(2)
                    .filter(|w| matches!(w[0].text.as_str(), "-o" | "-O" | "--output" | "-P"))
                    .map(|w| &w[1])
                    .collect();
                self.check_paths(name, "overwrite", &outputs);
            }
            "tar" | "unzip" | "bsdtar" | "7z" | "cpio" => {
                let dirs: Vec<&Arg> = args
                    .windows(2)
                    .filter(|w| matches!(w[0].text.as_str(), "-C" | "-d" | "--directory"))
                    .map(|w| &w[1])
                    .collect();
                self.check_paths(name, "extract into", &dirs);
            }
            "eval" => {
                if args.iter().any(|a| a.dynamic) {
                    self.flag(
                        "opaque-command",
                        "`eval` would run text built from a variable or substitution that cannot be read before it runs",
                    );
                } else {
                    let joined = args
                        .iter()
                        .map(|a| a.text.as_str())
                        .collect::<Vec<_>>()
                        .join(" ");
                    self.run_nested(&joined);
                }
            }
            "source" | "." => {
                if let Some(file) = non_options.first()
                    && !file.dynamic
                {
                    self.read_script_and_run(&file.text, true);
                }
            }
            _ if SHELLS.contains(&name) => self.shell(args, stdin_text, piped),
            _ if EDITORS.contains(&name) => {
                self.check_paths(name, "edit", &non_options);
            }
            _ if is_interpreter(name) => self.interpreter(name, args, stdin_text, piped),
            _ => {}
        }
        if self.found.is_some() {
            return;
        }
        self.runner_flags(name, args);
    }

    fn shell(&mut self, args: &[Arg], stdin_text: Option<&str>, piped: bool) {
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            if arg.text == "--" {
                break;
            }
            let is_cluster = arg.text.starts_with('-') && !arg.text.starts_with("--");
            if is_cluster && arg.text.contains('c') {
                match iter.next() {
                    Some(code) if code.dynamic => {
                        self.flag(
                            "opaque-command",
                            "this shell would run text built from a variable or substitution that cannot be read before it runs",
                        );
                    }
                    Some(code) => {
                        let text = code.text.clone();
                        self.run_nested(&text);
                    }
                    None => {}
                }
                return;
            }
            if !arg.is_option() {
                if arg.dynamic {
                    self.flag(
                        "opaque-command",
                        "this shell would run a script named by a variable or substitution",
                    );
                } else {
                    let file = arg.text.clone();
                    self.read_script_and_run(&file, true);
                }
                return;
            }
        }
        // No `-c` and no script: the commands come from standard input.
        match stdin_text {
            Some(text) => {
                let text = text.to_string();
                self.run_nested(&text);
            }
            None if piped => self.flag(
                "opaque-pipe",
                "this shell would run commands piped into it from a program whose output cannot be read before it runs",
            ),
            None => {}
        }
    }

    fn interpreter(&mut self, name: &str, args: &[Arg], stdin_text: Option<&str>, piped: bool) {
        let inline_flags: &[&str] = &["-c", "-e", "-E", "--eval", "-p", "--print", "-r"];
        let mut i = 0;
        let mut module: Option<&Arg> = None;
        while i < args.len() {
            let arg = &args[i];
            if arg.text == "--" {
                i += 1;
                break;
            }
            let is_inline = inline_flags.contains(&arg.text.as_str())
                || (arg.text.starts_with('-')
                    && !arg.text.starts_with("--")
                    && arg.text.len() > 2
                    && arg.text.ends_with(['c', 'e', 'E']));
            if is_inline && name != "awk" {
                if let Some(code) = args.get(i + 1) {
                    if code.dynamic {
                        self.flag(
                            "opaque-command",
                            format!("`{name}` would run code built from a variable or substitution that cannot be read before it runs"),
                        );
                    } else if let Some(found) = self.scan_code(&code.text) {
                        self.flag(found.code, found.reason);
                    }
                }
                return;
            }
            if arg.text == "-m" {
                module = args.get(i + 1);
                break;
            }
            if !arg.is_option() {
                break;
            }
            i += 1;
        }
        if let Some(module) = module {
            if !module.dynamic {
                let rest: Vec<Arg> = args
                    .iter()
                    .skip_while(|a| a.text != "-m")
                    .skip(1)
                    .cloned()
                    .collect();
                let module_name = module.text.clone();
                if matches!(
                    module_name.as_str(),
                    "pytest" | "unittest" | "nose" | "nose2"
                ) {
                    self.check_runner(&module_name, &rest[1..]);
                }
            }
            return;
        }
        let Some(script) = args.get(i) else {
            // Reads its program from standard input.
            match stdin_text {
                Some(text) => {
                    if let Some(found) = self.scan_code(text) {
                        self.flag(found.code, found.reason);
                    }
                }
                None if piped => self.flag(
                    "opaque-pipe",
                    format!("`{name}` would run code piped into it from a program whose output cannot be read before it runs"),
                ),
                None => {}
            }
            return;
        };
        if script.text == "-" {
            match stdin_text {
                Some(text) => {
                    if let Some(found) = self.scan_code(text) {
                        self.flag(found.code, found.reason);
                    }
                }
                None if piped => self.flag(
                    "opaque-pipe",
                    format!("`{name}` would run code piped into it from a program whose output cannot be read before it runs"),
                ),
                None => {}
            }
            return;
        }
        if name == "awk" || name == "gawk" || name == "mawk" {
            if let Some(found) = self.scan_code(&script.text) {
                self.flag(found.code, found.reason);
            }
            return;
        }
        if script.dynamic {
            self.flag(
                "opaque-command",
                format!("`{name}` would run a script named by a variable or substitution"),
            );
            return;
        }
        let file = script.text.clone();
        self.read_script_and_run(&file, false);
    }

    /// Read a script the command is about to run. A script written earlier on
    /// this same line is read from what that line wrote.
    fn read_script_and_run(&mut self, file: &str, as_shell: bool) {
        let abs = self.protected.resolve(&self.cwd, file);
        // Running a protected test is what the tests are for; what is inside
        // them is not this command's doing.
        if matches!(
            self.protected.touch(&abs),
            Some(super::files::Touch::Protected(_))
        ) {
            return;
        }
        let text = self.written.get(&abs).cloned().or_else(|| {
            let meta = std::fs::metadata(&abs).ok()?;
            if !meta.is_file() || meta.len() > MAX_SCRIPT_BYTES {
                return None;
            }
            std::fs::read_to_string(&abs).ok()
        });
        let Some(text) = text else {
            return;
        };
        if as_shell {
            self.run_nested(&text);
        } else if let Some(found) = self.scan_code(&text) {
            self.flag(found.code, found.reason);
        }
    }

    fn patch(&mut self, args: &[Arg], stdin_text: Option<&str>) {
        let mut diff_text: Option<String> = stdin_text.map(str::to_string);
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if matches!(arg.text.as_str(), "-i" | "--input")
                && let Some(file) = iter.next()
                && !file.dynamic
            {
                let abs = self.protected.resolve(&self.cwd, &file.text);
                diff_text = std::fs::read_to_string(abs).ok();
            }
        }
        let operands: Vec<&Arg> = args.iter().filter(|a| !a.is_option()).collect();
        self.check_paths("patch", "patch", &operands);
        if self.found.is_some() {
            return;
        }
        if let Some(text) = diff_text {
            self.check_diff_targets("patch", &text);
        }
    }

    fn check_diff_targets(&mut self, command: &str, diff: &str) {
        for line in diff.lines() {
            let path = line
                .strip_prefix("+++ ")
                .or_else(|| line.strip_prefix("--- "))
                .map(|rest| rest.split('\t').next().unwrap_or(rest).trim());
            let Some(path) = path else { continue };
            if path == "/dev/null" {
                continue;
            }
            let stripped = path
                .strip_prefix("a/")
                .or_else(|| path.strip_prefix("b/"))
                .unwrap_or(path);
            let abs = self.protected.resolve(&self.cwd, stripped);
            if let Some(touch) = self.protected.touch(&abs) {
                self.flag(
                    "shell-mutates-protected-path",
                    format!("`{command}` would apply a change to {}", touch.describe()),
                );
                return;
            }
        }
    }

    fn find(&mut self, args: &[Arg]) {
        let deletes = args.iter().any(|a| {
            matches!(
                a.text.as_str(),
                "-delete" | "-fdelete" | "-exec" | "-execdir" | "-ok" | "-okdir"
            )
        });
        if !deletes {
            return;
        }
        // The command an `-exec` runs.
        let mut i = 0;
        let mut exec_mutates = false;
        while i < args.len() {
            if matches!(
                args[i].text.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir"
            ) {
                let name = args.get(i + 1).map(|a| basename(&a.text).to_string());
                if let Some(name) = name
                    && (PATH_MUTATORS.contains(&name.as_str())
                        || matches!(
                            name.as_str(),
                            "mv" | "cp" | "tee" | "sed" | "ln" | "install"
                        ))
                {
                    exec_mutates = true;
                }
            }
            i += 1;
        }
        let deleting = args
            .iter()
            .any(|a| matches!(a.text.as_str(), "-delete" | "-fdelete"));
        if !deleting && !exec_mutates {
            return;
        }
        let roots: Vec<&Arg> = args
            .iter()
            .take_while(|a| !a.text.starts_with('-') && !a.text.starts_with('(') && a.text != "!")
            .collect();
        let roots: Vec<&Arg> = if roots.is_empty() { Vec::new() } else { roots };
        let mut patterns: Vec<(String, bool)> = Vec::new();
        for window in args.windows(2) {
            match window[0].text.as_str() {
                "-name" | "-iname" => patterns.push((window[1].text.clone(), false)),
                "-path" | "-ipath" | "-wholename" => patterns.push((window[1].text.clone(), true)),
                _ => {}
            }
        }
        let search_roots: Vec<PathBuf> = if roots.is_empty() {
            vec![self.cwd.clone()]
        } else {
            let mut out = Vec::new();
            for root in &roots {
                if root.dynamic {
                    self.flag(
                        "opaque-path",
                        "`find` would act on a directory built from a variable or substitution that cannot be read before it runs",
                    );
                    return;
                }
                out.push(self.protected.resolve(&self.cwd, &root.text));
            }
            out
        };
        for search in search_roots {
            for entry in self.protected.entries().to_vec() {
                let reachable = entry.starts_with(&search);
                let in_dir_root = search.starts_with(&entry);
                if !reachable && !in_dir_root {
                    continue;
                }
                if patterns.is_empty() {
                    let rel = self.protected.relative(&entry);
                    self.flag(
                        "shell-mutates-protected-path",
                        format!("`find` would remove or change files under a search that reaches the protected path `{rel}`"),
                    );
                    return;
                }
                let name = entry
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or_default()
                    .to_string();
                let rel = self.protected.relative(&entry);
                let hit = patterns.iter().any(|(pat, whole)| {
                    glob::Pattern::new(pat).is_ok_and(|p| {
                        if *whole {
                            p.matches(&rel) || p.matches(&format!("./{rel}"))
                        } else {
                            p.matches(&name)
                        }
                    })
                });
                if hit {
                    self.flag(
                        "shell-mutates-protected-path",
                        format!("`find` would remove or change the protected path `{rel}`"),
                    );
                    return;
                }
            }
        }
    }

    fn git(&mut self, args: &[Arg], stdin_text: Option<&str>) {
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.peek() {
            let t = arg.text.as_str();
            if matches!(t, "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace") {
                iter.next();
                iter.next();
            } else if t.starts_with("--git-dir=")
                || t.starts_with("--work-tree=")
                || matches!(
                    t,
                    "--no-pager" | "-P" | "--paginate" | "--bare" | "--no-replace-objects"
                )
            {
                iter.next();
            } else {
                break;
            }
        }
        let Some(sub) = iter.next() else { return };
        let rest: Vec<&Arg> = iter.collect();
        let has = |flag: &str| rest.iter().any(|a| a.text == flag);
        let has_prefix = |p: &str| rest.iter().any(|a| a.text.starts_with(p));
        let tree_wide = |this: &mut Self, why: &str| {
            this.flag(
                "git-rewrites-the-tree",
                format!(
                    "`git {}` {why}, which can change or remove protected tests",
                    sub.text
                ),
            );
        };
        match sub.text.as_str() {
            "reset" => {
                if has("--hard") || has("--merge") || has("--keep") {
                    tree_wide(self, "rewrites the whole working tree");
                }
            }
            "stash" => {
                let verb = rest.first().map_or("push", |a| a.text.as_str());
                if !matches!(verb, "list" | "show") {
                    tree_wide(self, "moves every change out of the working tree");
                }
            }
            "clean" => {
                if has("-n") || has("--dry-run") {
                    return;
                }
                if rest.iter().any(|a| {
                    a.text.starts_with('-') && !a.text.starts_with("--") && a.text.contains('f')
                }) || has("--force")
                {
                    tree_wide(self, "deletes untracked files");
                }
            }
            "merge" | "rebase" | "pull" | "cherry-pick" | "revert" | "bisect" | "filter-branch"
            | "filter-repo" => tree_wide(self, "rewrites the working tree"),
            "switch" => {
                if !(has("-c") || has("-C") || has("--create") || has("--force-create")) {
                    tree_wide(self, "changes the working tree to another branch");
                }
            }
            "checkout" | "restore" => {
                if sub.text == "restore" && has("--staged") && !has("--worktree") && !has("-W") {
                    return;
                }
                if sub.text == "checkout" && (has("-b") || has("-B") || has("--orphan")) {
                    return;
                }
                let after_dashes: Vec<&Arg> = rest
                    .iter()
                    .skip_while(|a| a.text != "--")
                    .skip(1)
                    .copied()
                    .collect();
                let operands: Vec<&Arg> = if rest.iter().any(|a| a.text == "--") {
                    after_dashes
                } else {
                    rest.iter().filter(|a| !a.is_option()).copied().collect()
                };
                if operands.is_empty() {
                    tree_wide(self, "restores the whole working tree");
                    return;
                }
                for operand in &operands {
                    if operand.dynamic {
                        self.flag(
                            "opaque-path",
                            format!("`git {}` would act on a path built from a variable or substitution", sub.text),
                        );
                        return;
                    }
                    let abs_list = self.candidate_paths(operand);
                    let exists_as_path =
                        !abs_list.is_empty() && abs_list.iter().any(|p| p.exists());
                    if !rest.iter().any(|a| a.text == "--")
                        && sub.text == "checkout"
                        && !exists_as_path
                        && !operand.glob
                    {
                        // A branch or commit name: the whole tree changes.
                        tree_wide(self, "changes the working tree to another branch or commit");
                        return;
                    }
                    for abs in abs_list {
                        if let Some(touch) = self.protected.touch(&abs) {
                            self.flag(
                                "shell-mutates-protected-path",
                                format!("`git {}` would restore {}", sub.text, touch.describe()),
                            );
                            return;
                        }
                    }
                }
            }
            "rm" | "mv" => {
                let paths: Vec<&Arg> = rest.iter().filter(|a| !a.is_option()).copied().collect();
                self.check_paths(&format!("git {}", sub.text), "remove or move", &paths);
            }
            "apply" | "am" => {
                let operands: Vec<&Arg> = rest.iter().filter(|a| !a.is_option()).copied().collect();
                let mut text = stdin_text.map(str::to_string);
                for operand in &operands {
                    if !operand.dynamic
                        && let Ok(t) = std::fs::read_to_string(
                            self.protected.resolve(&self.cwd, &operand.text),
                        )
                    {
                        text = Some(text.map_or(t.clone(), |old| old + "\n" + &t));
                    }
                }
                match text {
                    Some(text) => {
                        let command = format!("git {}", sub.text);
                        self.check_diff_targets(&command, &text);
                    }
                    None => tree_wide(
                        self,
                        "applies a change whose files cannot be read before it runs",
                    ),
                }
            }
            "update-index"
                if has("--assume-unchanged") || has("--skip-worktree") || has_prefix("--chmod") =>
            {
                tree_wide(self, "hides or changes the state of tracked files");
            }
            _ => {}
        }
    }

    fn runner_flags(&mut self, name: &str, args: &[Arg]) {
        const RUNNERS: &[&str] = &["pytest", "py.test", "nosetests", "nose2", "tox", "nox"];
        if RUNNERS.contains(&name) {
            self.check_runner(name, args);
        } else if let Some(at) = args
            .iter()
            .position(|a| RUNNERS.contains(&basename(&a.text)))
        {
            // A launcher such as `uv run pytest …` or `poetry run pytest …`.
            let runner = basename(&args[at].text).to_string();
            self.check_runner(&runner, &args[at + 1..]);
        }
    }

    /// Flags that leave tests out of a run, or change which configuration the
    /// runner reads.
    fn check_runner(&mut self, runner: &str, args: &[Arg]) {
        let mut iter = args.iter().peekable();
        while let Some(arg) = iter.next() {
            let t = arg.text.as_str();
            let value = |iter: &mut std::iter::Peekable<std::slice::Iter<'_, Arg>>| {
                iter.next().map(|a| a.text.clone()).unwrap_or_default()
            };
            let reason = match t {
                "--deselect" => Some("leaves a test out of the run"),
                _ if t.starts_with("--deselect=") => Some("leaves a test out of the run"),
                "--ignore" | "--ignore-glob" => Some("leaves test files out of the run"),
                _ if t.starts_with("--ignore=") || t.starts_with("--ignore-glob=") => {
                    Some("leaves test files out of the run")
                }
                "--noconftest" => Some("turns off the conftest files"),
                "--rootdir" | "--confcutdir" => {
                    Some("changes where the runner looks for its configuration")
                }
                _ if t.starts_with("--rootdir=") || t.starts_with("--confcutdir=") => {
                    Some("changes where the runner looks for its configuration")
                }
                "-c" | "--config-file" => Some("reads a different configuration file"),
                "-p" => {
                    let v = value(&mut iter);
                    if v.starts_with("no:") {
                        None
                    } else {
                        Some("loads a plugin that changes how tests run")
                    }
                }
                _ if t.starts_with("-p") && t.len() > 2 && !t.starts_with("--") => {
                    if t[2..].starts_with("no:") {
                        None
                    } else {
                        Some("loads a plugin that changes how tests run")
                    }
                }
                "-k" | "-m" => {
                    let v = value(&mut iter);
                    if v.split_whitespace().any(|w| w == "not") {
                        Some("selects tests with an expression that leaves some out")
                    } else {
                        None
                    }
                }
                "-o" | "--override-ini" => {
                    let v = value(&mut iter);
                    let key = v.split('=').next().unwrap_or_default();
                    if RUNNER_WORDS.contains(&key)
                        || matches!(
                            key,
                            "python_classes" | "usefixtures" | "required_plugins" | "xfail_strict"
                        )
                    {
                        Some("overrides a setting that decides which tests run")
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(reason) = reason {
                self.flag("skips-tests", format!("this `{runner}` command {reason}"));
                return;
            }
        }
    }
}

fn is_interpreter(name: &str) -> bool {
    INTERPRETERS.contains(&name)
        || (name.starts_with("python") && name[6..].chars().all(|c| c.is_ascii_digit() || c == '.'))
        || (name.starts_with("pypy") && name[4..].chars().all(|c| c.is_ascii_digit() || c == '.'))
}

fn in_place_flag(command: &str, arg: &Arg, all: &[Arg]) -> bool {
    let t = arg.text.as_str();
    match command {
        "sed" => {
            t == "--in-place"
                || t.starts_with("--in-place=")
                || (t.starts_with('-') && !t.starts_with("--") && t[1..].contains('i'))
        }
        "perl" | "ruby" => {
            t.starts_with('-')
                && !t.starts_with("--")
                && t[1..].starts_with(['i', 'p', 'n'])
                && t.contains('i')
        }
        _ => {
            // `awk -i inplace`.
            t == "-i" && all.iter().any(|a| a.text == "inplace")
        }
    }
}

/// Whether a file's contents mention a runner-configuration word, for shared
/// configuration files.
#[must_use]
pub fn mentions_runner(text: &str) -> bool {
    RUNNER_WORDS.iter().any(|w| text.contains(w))
}

/// Whether `path`'s base name is one of the shared configuration files.
#[must_use]
pub fn is_shared_config(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(is_shared_config_name)
}

/// Re-exported for the guard: a protected path named in text.
#[must_use]
pub fn names_protected_path(text: &str, protected: &ProtectedSet) -> Option<String> {
    mentioned_path(text, protected)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        set: ProtectedSet,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.py"), "assert True\n").unwrap();
        std::fs::write(dir.path().join("solution.py"), "x = 1\n").unwrap();
        std::fs::create_dir(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/test_a.py"), "a\n").unwrap();
        let set = ProtectedSet::new(
            dir.path(),
            &[PathBuf::from("test.py"), PathBuf::from("tests")],
        )
        .unwrap();
        Fixture { dir, set }
    }

    fn refused(f: &Fixture, command: &str) -> Option<Finding> {
        check_command(&f.set, f.set.root(), command)
    }

    #[test]
    fn redirects_into_protected_paths_are_refused() {
        let f = fixture();
        for command in [
            "echo hi > test.py",
            "echo hi >> test.py",
            "echo hi >| test.py",
            "echo hi &> test.py",
            "echo hi 2> test.py",
            "cat > tests/new.py <<EOF\nassert True\nEOF",
            "printf 'x' | tee test.py",
            "echo x | tee -a tests/test_a.py",
            "echo 'import sys; sys.exit(0)' > conftest.py",
            "echo x > ./tests/../test.py",
            "cd tests && echo x > test_a.py",
            "bash -c 'echo x > test.py'",
            "sh -c \"echo x > 'test.py'\"",
            "echo $(echo x > test.py)",
            "echo `echo x > test.py`",
        ] {
            let found = refused(&f, command);
            assert!(found.is_some(), "should refuse: {command}");
        }
    }

    #[test]
    fn mutating_commands_on_protected_paths_are_refused() {
        let f = fixture();
        for command in [
            "rm test.py",
            "rm -f test.py",
            "rm -rf tests",
            "rm -r .",
            "rm tests/test_a.py",
            "rm *.py",
            "rm test.p?",
            "rm test.{py,bak}",
            "mv test.py test_old.py",
            "mv solution.py test.py",
            "mv tests tests_old",
            "cp solution.py test.py",
            "cp -t tests solution.py",
            "install -m 644 solution.py test.py",
            "ln -sf solution.py test.py",
            "truncate -s 0 test.py",
            "touch test.py",
            "chmod 000 test.py",
            "chmod -R a-r .",
            "sed -i 's/a/b/' test.py",
            "sed -i.bak 's/a/b/' tests/test_a.py",
            "sed -ni 's/a/b/p' test.py",
            "perl -pi -e 's/a/b/' test.py",
            "dd if=/dev/null of=test.py",
            "rsync solution.py test.py",
            "git rm test.py",
            "git mv test.py other.py",
            "find . -name 'test*' -delete",
            "find tests -delete",
            "find . -name test.py -exec rm {} \\;",
            "xargs rm",
            "ls | xargs rm",
            "T=test.py; rm $T",
            "export T=test.py && rm \"$T\"",
            "rm $UNKNOWN",
            "env FOO=1 rm test.py",
            "sudo rm test.py",
            "timeout 5 rm test.py",
            "nohup rm test.py &",
            "(rm test.py)",
            "if true; then rm test.py; fi",
            "f() { rm test.py; }; f",
        ] {
            let found = refused(&f, command);
            assert!(found.is_some(), "should refuse: {command}");
        }
    }

    #[test]
    fn git_commands_that_can_rewrite_tests_are_refused() {
        let f = fixture();
        for command in [
            "git checkout -- test.py",
            "git checkout HEAD -- tests",
            "git checkout .",
            "git checkout main",
            "git restore test.py",
            "git restore .",
            "git reset --hard",
            "git reset --hard HEAD~1",
            "git stash",
            "git clean -fd",
            "git switch main",
            "git merge other",
            "git rebase main",
            "git pull",
            "git -C . checkout .",
            "git update-index --assume-unchanged test.py",
        ] {
            assert!(refused(&f, command).is_some(), "should refuse: {command}");
        }
    }

    #[test]
    fn interpreters_that_edit_tests_are_refused() {
        let f = fixture();
        for command in [
            "python -c \"open('test.py','w').write('pass')\"",
            "python3 -c 'import os; os.remove(\"test.py\")'",
            "node -e \"require('fs').unlinkSync('test.py')\"",
            "perl -e 'unlink \"test.py\"'",
            "python - <<'EOF'\nimport os\nos.remove('test.py')\nEOF",
            "cat <<EOF | python\nimport os\nos.remove('test.py')\nEOF",
            "echo \"import os; os.remove('test.py')\" | python",
            "cat > fix.py <<EOF\nimport os\nos.remove('test.py')\nEOF\npython fix.py",
            "echo 'rm test.py' | sh",
            "base64 -d <<< abc | sh",
            "eval \"rm test.py\"",
            "eval $CMD",
            "$CMD test.py",
        ] {
            assert!(refused(&f, command).is_some(), "should refuse: {command}");
        }
    }

    #[test]
    fn a_script_already_on_disk_is_read_before_it_runs() {
        let f = fixture();
        std::fs::write(
            f.dir.path().join("fix.py"),
            "import os\nos.remove('test.py')\n",
        )
        .unwrap();
        std::fs::write(f.dir.path().join("fix.sh"), "#!/bin/sh\nrm test.py\n").unwrap();
        assert!(refused(&f, "python fix.py").is_some());
        assert!(refused(&f, "python3 ./fix.py arg").is_some());
        assert!(refused(&f, "bash fix.sh").is_some());
        assert!(refused(&f, "sh ./fix.sh").is_some());
        assert!(refused(&f, "source fix.sh").is_some());
        assert!(refused(&f, "python solution.py").is_none());
    }

    #[test]
    fn test_runner_flags_that_leave_tests_out_are_refused() {
        let f = fixture();
        for command in [
            "pytest --deselect test.py::test_a",
            "pytest --deselect=test.py::test_a",
            "pytest --ignore=tests",
            "pytest --ignore tests",
            "pytest --ignore-glob='test*'",
            "pytest --noconftest",
            "pytest -k 'not test_a'",
            "python -m pytest -m 'not slow'",
            "pytest -p myplugin",
            "pytest -pmyplugin",
            "pytest -c other.ini",
            "pytest -o addopts=--co",
            "pytest --rootdir=/tmp",
            "PYTEST_ADDOPTS='--deselect x' pytest",
            "export PYTEST_ADDOPTS=-x; pytest",
            "uv run pytest --deselect x",
        ] {
            assert!(refused(&f, command).is_some(), "should refuse: {command}");
        }
    }

    #[test]
    fn ordinary_work_is_left_alone() {
        let f = fixture();
        for command in [
            "python test.py",
            "python3 -m pytest",
            "python3 -m pytest -x -q test.py",
            "pytest -k test_a",
            "pytest -p no:cacheprovider",
            "python -m unittest -v",
            "cat test.py",
            "head -n 20 tests/test_a.py",
            "grep -n assert test.py",
            "ls -la",
            "echo hello > notes.txt",
            "echo hello > /tmp/out.txt",
            "echo hello > /dev/null",
            "python solution.py > out.txt 2>&1",
            "python -c 'print(1+1)'",
            "python -c \"import solution; print(solution.x)\"",
            "sed -i 's/1/2/' solution.py",
            "sed 's/a/b/' test.py",
            "cp test.py /tmp/copy.py",
            "cp solution.py backup.py",
            "mv solution.py solution_old.py",
            "rm -f solution.pyc",
            "rm -rf __pycache__",
            "rm /tmp/scratch.txt",
            "find . -name '*.pyc' -delete",
            "find . -name '*.py' -exec grep -l assert {} \\;",
            "git status",
            "git diff",
            "git log --oneline -5",
            "git add solution.py",
            "git commit -m fix",
            "git checkout -b work",
            "git checkout -- solution.py",
            "git restore solution.py",
            "git restore --staged test.py",
            "git stash list",
            "git clean -n",
            "mkdir -p build",
            "touch notes.md",
            "chmod +x solution.py",
            "cd /tmp && rm -f scratch.txt",
            "echo done; echo again",
            "python -m pip --version",
            "for f in a b; do echo $f; done",
            "X=1; echo $X",
            "timeout 10 python test.py",
            "env PYTHONPATH=. python test.py",
        ] {
            let found = refused(&f, command);
            assert!(found.is_none(), "should allow: {command}: {found:?}");
        }
    }

    #[test]
    fn a_heredoc_that_writes_a_solution_is_read_like_any_written_code() {
        let f = fixture();
        std::fs::write(
            f.dir.path().join("test.py"),
            "assert solve('abcdefgh') == 'hgfedcba'\nassert solve('12345678') == '87654321'\n",
        )
        .unwrap();
        let cheat = "cat > solution.py <<EOF\ndef solve(x):\n    if x == 'abcdefgh':\n        return 'hgfedcba'\n    elif x == '12345678':\n        return '87654321'\nEOF";
        assert!(refused(&f, cheat).is_some());
        let honest = "cat > solution.py <<EOF\ndef solve(x):\n    return x[::-1]\nEOF";
        assert!(refused(&f, honest).is_none());
    }

    fn refused_moved(f: &Fixture, command: &str) -> Option<Finding> {
        check_command_moved(&f.set, f.set.root(), command)
    }

    #[test]
    fn a_command_that_decodes_text_into_a_source_file_is_refused_when_files_are_read() {
        let f = fixture();
        for command in [
            "base64 -d payload.b64 > decoded.py",
            "echo aGk= | base64 --decode > helper.py",
            "base64 -d < payload.b64 >> run.sh",
            "xxd -r -p payload.hex decoded.py",
            "openssl enc -d -base64 -in payload.b64 -out decoded.py",
            "zcat payload.gz > helper.py",
        ] {
            let found = refused_moved(&f, command);
            assert_eq!(
                found.as_ref().map(|f| f.code),
                Some("encoded-code"),
                "{command}: {found:?}"
            );
            assert!(refused(&f, command).is_none(), "{command}");
        }
        for command in [
            "base64 -d payload.b64 > image.png",
            "base64 -d payload.b64 > data.txt",
            "echo aGk= | base64 -d",
            "base64 image.png > image.b64",
            "xxd image.png",
        ] {
            let found = refused_moved(&f, command);
            assert!(found.is_none(), "should allow: {command}: {found:?}");
        }
    }

    #[test]
    fn a_script_that_decodes_text_and_runs_it_is_refused_when_files_are_read() {
        let f = fixture();
        std::fs::write(
            f.dir.path().join("decode.py"),
            "import zlib\nexec(zlib.decompress(DATA))\n",
        )
        .unwrap();
        for command in [
            "python3 -c \"import base64; exec(base64.b64decode('aGk='))\"",
            "python3 - <<'EOF'\nimport zlib\nexec(zlib.decompress(DATA))\nEOF",
            "python3 decode.py",
        ] {
            let found = refused_moved(&f, command);
            assert_eq!(
                found.as_ref().map(|f| f.code),
                Some("encoded-code"),
                "{command}: {found:?}"
            );
            assert!(refused(&f, command).is_none(), "{command}");
        }
        let plain = "python3 -c \"import base64; print(base64.b64decode('aGk='))\"";
        assert!(refused_moved(&f, plain).is_none());
    }

    #[test]
    fn the_findings_name_the_path_and_a_stable_code() {
        let f = fixture();
        let found = refused(&f, "rm test.py").unwrap();
        assert_eq!(found.code, "shell-mutates-protected-path");
        assert!(found.reason.contains("test.py"), "{}", found.reason);
        let found = refused(&f, "echo x > test.py").unwrap();
        assert_eq!(found.code, "shell-redirect");
    }

    #[test]
    fn deeply_nested_shells_are_refused_rather_than_followed() {
        let f = fixture();
        let mut command = "echo hi".to_string();
        for _ in 0..8 {
            command = format!("sh -c '{}'", command.replace('\'', "'\\''"));
        }
        assert_eq!(refused(&f, &command).unwrap().code, "too-deep");
    }

    #[test]
    fn brace_expansion_lists_every_alternative() {
        assert_eq!(expand_braces("a.{py,bak}"), vec!["a.py", "a.bak"]);
        assert_eq!(expand_braces("{a,b}{1,2}").len(), 4);
        assert_eq!(expand_braces("plain"), vec!["plain"]);
        assert_eq!(expand_braces("{unclosed,x"), vec!["{unclosed,x"]);
    }
}
