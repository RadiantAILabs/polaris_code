//! Conservative shell decomposition for session command allow lists.
//!
//! This recognizes simple commands, chains, pipes, and literal here documents.
//! Unsupported shell grammar fails closed. Interpreter source is data, not shell
//! syntax; the interpreter's own capabilities remain governed by its sandbox.

use polaris::shell::{ShellConfig, ShellPermission};

/// Return the checked shell commands, excluding here-document data.
///
/// Every returned part must match an allow pattern using the framework's byte
/// wildcard dialect (`*` and `?`). Newlines in quoted arguments are preserved.
/// Here-document delimiters may be quoted; unquoted bodies must contain no
/// expansion. Tab-stripping (`<<-`), multiple pending here documents, control
/// bytes other than tab/newline, and backslash-newline are refused everywhere,
/// including inside quoted arguments, delimiters, and here-document data.
/// Command words must be literal, leading assignments/redirections, shell
/// control forms, substitutions, and line continuations are refused.
#[must_use]
pub fn checked_commands(command: &str, patterns: &[String]) -> Option<Vec<String>> {
    let parts = decompose(command)?;
    (!parts.is_empty()
        && parts
            .iter()
            .all(|part| patterns.iter().any(|pattern| wildcard_match(pattern, part))))
    .then_some(parts)
}

/// Match one already parsed command against the executor's literal patterns.
/// Grammar validation must precede this; an unparsed command is not a part.
pub(crate) fn part_permission(command: &str, config: &ShellConfig) -> ShellPermission {
    if config
        .denied_commands
        .iter()
        .any(|pattern| wildcard_match(pattern, command))
    {
        ShellPermission::Deny
    } else if config
        .allowed_commands
        .iter()
        .any(|pattern| wildcard_match(pattern, command))
    {
        ShellPermission::Allow
    } else {
        ShellPermission::Confirm
    }
}

// The shell's patterns operate on bytes, with only * and ? special. Use a
// bounded dynamic program rather than interpreting quote bytes as glob escapes.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let mut matches = vec![false; text.len() + 1];
    matches[0] = true;
    for pattern_byte in pattern.bytes() {
        let mut previous = matches[0];
        matches[0] &= pattern_byte == b'*';
        for (index, text_byte) in text.bytes().enumerate() {
            let old = matches[index + 1];
            matches[index + 1] = if pattern_byte == b'*' {
                old || matches[index]
            } else {
                previous && (pattern_byte == b'?' || pattern_byte == text_byte)
            };
            previous = old;
        }
    }
    matches[text.len()]
}

fn decompose(command: &str) -> Option<Vec<String>> {
    let bytes = command.as_bytes();
    // Validate the whole input before skipping literal bodies or escaped bytes.
    // Bash's treatment of control bytes and continuations depends on where
    // they occur; this parser deliberately does not model those combinations.
    if bytes
        .iter()
        .any(|b| b.is_ascii_control() && !matches!(b, b'\t' | b'\n'))
        || bytes.windows(2).any(|pair| pair == b"\\\n")
    {
        return None;
    }
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut quote = None;
    let mut redirect = false;
    let mut heredoc: Option<(String, bool)> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if quote.is_none() && b == b'$' && matches!(bytes.get(i + 1), Some(b'\'' | b'"')) {
            return None;
        }
        if b == b'\\' && quote != Some(b'\'') {
            let next = *bytes.get(i + 1)?;
            if next == b'\n' {
                return None;
            }
            // Preserve the full UTF-8 character, including an escaped one.
            let ch = command[i + 1..].chars().next()?;
            part.push('\\');
            part.push(ch);
            redirect = false;
            i += 1 + ch.len_utf8();
            continue;
        }
        if quote != Some(b'\'')
            && (b == b'`' || (b == b'$' && matches!(bytes.get(i + 1), Some(b'(' | b'{' | b'['))))
        {
            return None;
        }
        if (b == b'\'' || b == b'"') && (quote.is_none() || quote == Some(b)) {
            quote = if quote.is_some() { None } else { Some(b) };
        } else if quote.is_none() {
            match b {
                b'(' | b')' | b'{' | b'}' | b'#' => return None,
                b'<' if bytes.get(i + 1) == Some(&b'<') => {
                    // The '-' in <<- is an operator, not a delimiter byte.
                    // Refuse this grammar rather than silently swallowing
                    // commands after Bash's tab-stripped terminator.
                    if bytes.get(i + 2) == Some(&b'-')
                        || heredoc.is_some()
                        || part.trim_matches([' ', '\t']).is_empty()
                    {
                        return None;
                    }
                    i += 2;
                    while matches!(bytes.get(i), Some(b' ' | b'\t')) {
                        i += 1;
                    }
                    let quoted = matches!(bytes.get(i), Some(b'\'' | b'"'));
                    let delimiter_quote = quoted.then(|| bytes[i]);
                    if quoted {
                        i += 1;
                    }
                    let start = i;
                    if let Some(q) = delimiter_quote {
                        while let Some(&c) = bytes.get(i) {
                            if c == q {
                                break;
                            }
                            if matches!(c, 0 | b'\n' | b'\r') || (q == b'"' && c == b'\\') {
                                return None;
                            }
                            i += 1;
                        }
                    } else {
                        while bytes
                            .get(i)
                            .is_some_and(|c| c.is_ascii_alphanumeric() || b"_.-".contains(c))
                        {
                            i += 1;
                        }
                    }
                    if start == i {
                        return None;
                    }
                    let delimiter = command[start..i].to_owned();
                    if let Some(q) = delimiter_quote {
                        if bytes.get(i) != Some(&q) {
                            return None;
                        }
                        i += 1;
                    }
                    if !matches!(
                        bytes.get(i),
                        None | Some(b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'>' | b'<')
                    ) {
                        return None;
                    }
                    heredoc = Some((delimiter, quoted));
                    redirect = false;
                    part.push(' ');
                    continue;
                }
                b';' | b'|' | b'&' | b'\n' => {
                    // Descriptor duplication is part of a redirection, not a
                    // command separator. Other ampersands split commands.
                    if b == b'&' && redirect {
                        part.push('&');
                        redirect = false;
                        i += 1;
                        continue;
                    }
                    if !part.trim_matches([' ', '\t']).is_empty() {
                        push_part(&mut parts, &mut part)?;
                    } else if b != b'\n' {
                        return None;
                    }
                    redirect = false;
                    i += 1;
                    if matches!(b, b'|' | b'&') && bytes.get(i) == Some(&b) {
                        i += 1;
                    }
                    if b == b'\n'
                        && let Some((delimiter, quoted)) = heredoc.take()
                    {
                        let body_start = i;
                        loop {
                            let end = command[i..]
                                .find('\n')
                                .map_or(bytes.len(), |offset| i + offset);
                            if command[i..end] == delimiter {
                                let body = &command[body_start..i];
                                if !quoted && (body.contains(['$', '`', '\\'])) {
                                    return None;
                                }
                                i = if end < bytes.len() { end + 1 } else { end };
                                break;
                            }
                            if end == bytes.len() {
                                return None;
                            }
                            i = end + 1;
                        }
                    }
                    continue;
                }
                _ => {}
            }
        }
        let ch = command[i..].chars().next()?;
        part.push(ch);
        redirect = quote.is_none() && matches!(b, b'<' | b'>');
        i += ch.len_utf8();
    }
    if quote.is_some() || heredoc.is_some() {
        return None;
    }
    if !part.trim_matches([' ', '\t']).is_empty() {
        push_part(&mut parts, &mut part)?;
    }
    Some(parts)
}

fn push_part(parts: &mut Vec<String>, part: &mut String) -> Option<()> {
    let text = part.trim_matches([' ', '\t']);
    let word = text.split([' ', '\t']).next()?;
    if word.bytes().all(|byte| byte.is_ascii_digit())
        || !word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./-".contains(&b))
        || matches!(
            word,
            "if" | "then"
                | "else"
                | "elif"
                | "fi"
                | "for"
                | "select"
                | "coproc"
                | "time"
                | "while"
                | "until"
                | "do"
                | "done"
                | "case"
                | "esac"
                | "in"
                | "function"
                | "eval"
                | "exec"
                | "source"
                | "."
                | "!"
        )
    {
        return None;
    }
    parts.push(text.to_owned());
    part.clear();
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn check(command: &str) -> bool {
        checked_commands(
            command,
            &[
                "python3 *".into(),
                "cat *".into(),
                "head *".into(),
                "head".into(),
                "cd *".into(),
            ],
        )
        .is_some()
    }
    #[test]
    fn accepts_literal_scripts_and_checks_every_shell_command() {
        for command in [
            "cd x && python3 run.py | head -20",
            "python3 - <<'EOF'\nprint('$(unlisted)'); print(`data`)\nEOF\ncat result.txt",
            "python3 - <<\"EOF\" | head -2\nprint('hello')\nEOF\n",
            "python3 - <<EOF\nprint('hello')\nEOF",
            "python3 -c \"import json\nprint('hello')\"",
            "python3 -c 'print(1)\nprint(2)'\ncat output.txt",
            "cat a 2>&1 | head -10",
            "cat .",
            "cat a | head",
            "python3 - <<'END SCRIPT'\nprint(1)\nEND SCRIPT",
            "python3 -c \"source = 1\nprint(source)\"",
        ] {
            assert!(check(command), "{command}");
        }
    }
    #[test]
    fn refuses_opaque_or_unlisted_commands() {
        for command in [
            "\u{a0}cat x",
            "cat x\n\u{a0}unlisted x",
            "cat \\>& unlisted x",
            "cat \\<& unlisted x",
            "cat x && unlisted x",
            "cat x | unlisted x",
            "python3 - <<EOF\n$(unlisted)\nEOF",
            "python3 - <<EOF\n$VALUE\nEOF",
            "python3 - <<EOF\n`unlisted`\nEOF",
            "python3 - <<EOF\n\\\nEOF\nunlisted\nEOF",
            "python3 - <<'EOF'\nprint(1)\nEOF\nunlisted x",
            ">output cat x",
            "VAR=x cat x",
            "cat $(unlisted)",
            "cat \"${PWD@P}\"",
            "cat \"$[index]\"",
            "cat `unlisted`",
            "cat <(unlisted)",
            "cat /dev/null >(unlisted)",
            "cat /dev/null >$(unlisted)",
            "cat /dev/null >`unlisted`",
            "cat /dev/null 2>&1\n/usr/bin/printf UNLISTED",
            "cat \"<<'EOF'\nEOF\n\"; /usr/bin/printf UNLISTED",
            "cat '<<EOF\nEOF\n'; /usr/bin/printf UNLISTED",
            "cat x\nunlisted x",
            "cat $\\\n(unlisted)",
            "python3 - <<'EOF'\nprint(1)",
            "python3 - <<-'EOF'\nprint(1)\nEOF",
            "python3 - <<'EOF' <<'NEXT'\nEOF\nNEXT",
            "cat x # ' \nunlisted x",
            "cat x; if cat x; then unlisted; fi",
            "cat x &>out unlisted",
            "cat $'ansi quoting'",
            "'cat' x",
            "time cat x",
            "coproc cat x",
            "select x in y; do cat x; done",
        ] {
            assert!(!check(command), "{command}");
        }
        assert!(checked_commands("cat x", &[]).is_none());
        assert!(checked_commands("2<<'EOF' unlisted x\ndata\nEOF", &["2 *".into()]).is_none());
        for command in ["time unlisted", "coproc unlisted", "select x in y"] {
            assert!(checked_commands(command, &["*".into()]).is_none());
        }
    }

    #[test]
    fn here_documents_end_only_at_the_exact_literal_delimiter() {
        let patterns = vec!["cat *".into()];
        for delimiter in ["EOF", "'EOF'", "\"EOF\"", " -EOF", "'-EOF'", "\"-EOF\""] {
            let end = if delimiter.contains('-') {
                "-EOF"
            } else {
                "EOF"
            };
            // Near matches, quoted occurrences, and apparent nested documents
            // remain data until an exact whole-line terminator occurs.
            let command = format!(
                "cat /dev/null <<{delimiter}\n{end} \n{end} extra\n'{end}'\n\"{end}\"\ncat <<INNER\nINNER\n/usr/bin/printf BODY_ONLY\n{end}\n"
            );
            assert_eq!(
                checked_commands(&command, &patterns),
                Some(vec!["cat /dev/null".into()]),
                "{command:?}"
            );
            assert!(
                checked_commands(&format!("{command}/usr/bin/printf UNLISTED\n"), &patterns)
                    .is_none(),
                "{command:?}"
            );
            for bad_end in [
                format!("{end} "),
                format!("{end} extra"),
                format!("\t{end}"),
            ] {
                let missing = format!("cat /dev/null <<{delimiter}\n{bad_end}\n");
                assert!(
                    checked_commands(&missing, &patterns).is_none(),
                    "{missing:?}"
                );
            }
        }
    }

    #[test]
    fn refuses_tab_stripping_multiple_documents_and_partial_delimiter_quotes() {
        for command in [
            "cat /dev/null <<-EOF\nEOF\n/usr/bin/printf UNLISTED\n-EOF\n",
            "cat /dev/null <<-EOF\n\tEOF\n/usr/bin/printf UNLISTED\n-EOF\n",
            "cat /dev/null <<--EOF\n-EOF\n/usr/bin/printf UNLISTED\n--EOF\n",
            "cat /dev/null <<-'EOF'\n\tEOF\n",
            "cat /dev/null <<-\"EOF\"\n\tEOF\n",
            "cat /dev/null <<- 'EOF'\nEOF\n",
            "cat /dev/null <<EOF <<NEXT\nEOF\nNEXT\n",
            "cat /dev/null <<'EOF' <<\"NEXT\"\nEOF\nNEXT\n",
            "cat /dev/null <<EOF; cat /dev/null <<NEXT\nEOF\nNEXT\n",
            "cat /dev/null <<E'OF'\nEOF\n",
            "cat /dev/null <<'EO'F\nEOF\n",
            "cat /dev/null <<\\EOF\nEOF\n",
            "cat /dev/null <<\"E\\OF\"\nEOF\n",
        ] {
            assert!(!check(command), "{command:?}");
        }
    }

    #[test]
    fn refuses_control_bytes_and_continuations_even_in_skipped_data() {
        for unsupported in (0_u8..=127)
            .filter(|byte| byte.is_ascii_control() && !matches!(byte, b'\t' | b'\n'))
            .map(|byte| char::from(byte).to_string())
            .chain(["\\\n".into()])
        {
            for command in [
                format!("cat /dev/null{unsupported}"),
                format!("cat '/dev/null{unsupported}'"),
                format!("cat \"/dev/null{unsupported}\""),
                format!("cat /dev/null <<'EOF'\nEOF{unsupported}\nEOF\n"),
                format!("cat /dev/null <<\"EOF\"\nEOF{unsupported}\nEOF\n"),
                format!("cat /dev/null <<EOF\nEOF{unsupported}\nEOF\n"),
                format!("cat /dev/null <<'E{unsupported}OF'\nE{unsupported}OF\n"),
            ] {
                assert!(!check(&command), "{command:?}");
            }
        }
    }

    #[cfg(unix)]
    fn bash_trace(command: &str) -> Vec<String> {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("bash")
            .args(["--noprofile", "--norc", "-x", "-c", command])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .current_dir(directory.path())
            .output()
            .expect("Bash must be installed for the shell permission regression tests");
        String::from_utf8(output.stderr)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("+ ").map(str::to_owned))
            .collect()
    }

    #[test]
    #[cfg(unix)]
    fn bash_runs_the_marker_after_each_tab_stripping_terminator() {
        // Fixed harmless probes only: no user input, credentials, network, or
        // persistent writes. The false terminator becomes a missing command.
        for (operator, end, false_end) in [
            ("<<-EOF", "EOF", "-EOF"),
            ("<<-EOF", "\tEOF", "-EOF"),
            ("<<--EOF", "-EOF", "--EOF"),
            ("<<-'EOF'", "\tEOF", "-EOF"),
            ("<<-\"EOF\"", "\tEOF", "-EOF"),
        ] {
            let command =
                format!("cat /dev/null {operator}\n{end}\n/usr/bin/printf UNLISTED\n{false_end}\n");
            assert!(bash_trace(&command).contains(&"/usr/bin/printf UNLISTED".into()));
            assert!(checked_commands(&command, &["cat *".into()]).is_none());
        }
    }

    #[test]
    #[cfg(unix)]
    fn accepted_here_document_parts_match_bash_execution() {
        let patterns = vec!["cat *".into(), "/usr/bin/printf CHECKED".into()];
        for (delimiter, end, body) in [
            ("EOF", "EOF", "hello"),
            ("'EOF'", "EOF", "$(/usr/bin/printf BODY_ONLY)"),
            ("\"EOF\"", "EOF", "`/usr/bin/printf BODY_ONLY`"),
            (" -EOF", "-EOF", "hello"),
            ("'-EOF'", "-EOF", "hello"),
            ("\"-EOF\"", "-EOF", "hello"),
            ("'END SCRIPT'", "END SCRIPT", "hello"),
        ] {
            let command = format!(
                "cat /dev/null <<{delimiter}\n{end} \n'{end}'\n\"{end}\"\n{end} other\n{body}\n{end}\n/usr/bin/printf CHECKED"
            );
            let parts = checked_commands(&command, &patterns).unwrap();
            assert_eq!(parts, bash_trace(&command), "{command:?}");
            assert!(checked_commands(&command, &["cat *".into()]).is_none());
        }
        for command in [
            "cat /dev/null <<EOF\nhello\nEOF",
            "cat /dev/null <<'EOF'\nhello\nEOF",
            "cat /dev/null <<\"EOF\"\nhello\nEOF",
            "cat /dev/null <<EOF\none\nEOF\ncat /dev/null <<'NEXT'\ntwo\nNEXT\n",
        ] {
            assert_eq!(
                checked_commands(command, &patterns).unwrap(),
                bash_trace(command),
                "{command:?}"
            );
        }
    }
    #[test]
    fn read_utilities_work_alone_or_in_a_pipeline_only_when_listed() {
        for program in ["ls", "grep", "head", "tail", "wc", "sort", "uniq"] {
            let patterns = vec!["cat *".into(), program.into(), format!("{program} *")];
            for command in [
                program.to_owned(),
                format!("{program} file"),
                format!("cat file | {program}"),
            ] {
                assert!(checked_commands(&command, &patterns).is_some(), "{command}");
                assert!(
                    checked_commands(&command, &["cat *".into()]).is_none(),
                    "{command}"
                );
            }
        }
    }

    #[test]
    fn literal_pattern_dialect_matches_the_framework() {
        use polaris::shell::ShellExecutor;
        for pattern in [
            "*",
            "cat *",
            "cat ?",
            "cat [a]",
            "cat a\\b",
            "cat a?*b",
            "cat **x**",
        ] {
            let config = ShellConfig::new().with_allowed_commands(vec![pattern.into()]);
            let executor = ShellExecutor::new(config.clone());
            for command in [
                "cat x",
                "cat ab",
                "cat [a]",
                "cat a\\b",
                "cat axxb",
                "cat café",
                "head x",
            ] {
                assert_eq!(
                    part_permission(command, &config),
                    executor.check_permission(command),
                    "{pattern:?}: {command:?}"
                );
            }
        }
        let config = ShellConfig::new()
            .with_allowed_commands(vec!["cat *".into()])
            .with_denied_commands(vec!["cat secret*".into()]);
        assert_eq!(
            part_permission("cat secret.txt", &config),
            ShellPermission::Deny
        );
    }
}
