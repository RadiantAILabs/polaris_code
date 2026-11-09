//! Code kept in an encoded form and turned back into code when it runs.
//!
//! A program that is stored as base64 or hex, or decoded and then run or written
//! into a source file, cannot be read by the checks that read source. Honest
//! work has no reason to do this: the code can be written out. Two shapes are
//! found, both from the text alone:
//!
//! - text that holds an encoded blob which decodes to a Python program;
//! - a script that decodes text and, a few lines away, runs it (`exec`, `eval`,
//!   `compile`, an import by name) or writes it into a `.py` file.
//!
//! A shell command that decodes into a source file is found by
//! [`is_shell_decoder`] together with the file it writes.

use std::sync::OnceLock;

use regex::Regex;

use super::py_scan::is_python_program;

/// Code kept encoded and decoded to run.
pub const CODE_ENCODED: &str = "encoded-code";

/// Longest blob that is decoded and read.
const MAX_BLOB_BYTES: usize = 1 << 20;

/// How many lines apart a decoder and the code that runs or stores its output
/// may be. Far apart they are, in a long file, different jobs.
const NEAR_LINES: usize = 12;

/// Most candidate blobs tried in one text.
const MAX_CANDIDATES: usize = 64;

/// Shortest run of characters that is tried as an encoded blob.
const MIN_BLOB_CHARS: usize = 24;

/// File extensions of code a decoder could write.
const CODE_EXTENSIONS: &[&str] = &[".py", ".sh", ".bash", ".js", ".mjs", ".rb", ".pl"];

fn blob_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9+/_-]{24,}={0,2}").expect("blob pattern compiles"))
}

fn hex_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:[0-9a-fA-F]{2}){16,}").expect("hex pattern compiles"))
}

fn decoder_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"\b(?:b64decode|b32decode|b16decode|urlsafe_b64decode|standard_b64decode|a2b_base64|a2b_hex|a2b_uu|decodebytes|unhexlify|fromhex)\b|\b(?:zlib|gzip|bz2|lzma)\.decompress\b|\bcodecs\.decode\b|\.decode\(\s*['"](?:base64|hex|zlib|rot13|rot_13|uu)"#,
        )
        .expect("decoder pattern compiles")
    })
}

fn run_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?:^|[^.\w])(?:exec|eval|compile)\s*\(|__import__\s*\(|\bimportlib\b|\brunpy\b|\bspec_from_loader\b",
        )
        .expect("run pattern compiles")
    })
}

fn write_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\.(?:write_bytes|write_text|write)\s*\(").expect("write pattern compiles")
    })
}

fn python_file_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\.py['"]"#).expect("python file pattern compiles"))
}

/// The bytes `blob` stands for in base64, in either alphabet, with or without
/// padding.
fn decode_base64(blob: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(blob.len() * 3 / 4);
    let mut bits = 0_u32;
    let mut count = 0_u32;
    let mut seen = 0_usize;
    for c in blob.trim_end_matches('=').chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' | '-' => 62,
            '/' | '_' => 63,
            _ => return None,
        };
        seen += 1;
        bits = (bits << 6) | value;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xff).ok()?);
            bits &= (1 << count) - 1;
        }
    }
    (seen % 4 != 1).then_some(out)
}

fn decode_hex(blob: &str) -> Option<Vec<u8>> {
    let bytes = blob.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

fn decodes_to_program(bytes: Vec<u8>) -> bool {
    String::from_utf8(bytes).is_ok_and(|text| is_python_program(&text))
}

/// Whether `text` holds a blob that is a Python program in base64 or hex.
fn holds_encoded_program(text: &str) -> bool {
    if text.len() > MAX_BLOB_BYTES {
        return false;
    }
    // The whole text as one blob, as `base64` writes it across lines.
    let joined: String = text.split_whitespace().collect();
    if joined.len() >= MIN_BLOB_CHARS
        && joined
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '_' | '-' | '='))
        && decode_base64(&joined).is_some_and(decodes_to_program)
    {
        return true;
    }
    let mut tried = 0;
    for found in blob_pattern().find_iter(text) {
        tried += 1;
        if tried > MAX_CANDIDATES {
            return false;
        }
        if decode_base64(found.as_str()).is_some_and(decodes_to_program) {
            return true;
        }
    }
    hex_pattern()
        .find_iter(text)
        .take(MAX_CANDIDATES)
        .any(|found| decode_hex(found.as_str()).is_some_and(decodes_to_program))
}

/// The line numbers `pattern` matches on, the first `MAX_CANDIDATES` of them.
fn lines_of(pattern: &Regex, text: &str) -> Vec<usize> {
    pattern
        .find_iter(text)
        .take(MAX_CANDIDATES)
        .map(|found| text[..found.start()].matches('\n').count())
        .collect()
}

fn near(a: usize, b: usize) -> bool {
    a.abs_diff(b) <= NEAR_LINES
}

/// What `text` does with encoded code, in a sentence that completes "this
/// code …", or `None`.
#[must_use]
pub fn encoded_code(text: &str) -> Option<&'static str> {
    let decoders = lines_of(decoder_pattern(), text);
    if !decoders.is_empty() {
        let runs = lines_of(run_pattern(), text);
        if decoders.iter().any(|d| runs.iter().any(|r| near(*d, *r))) {
            return Some(
                "decodes text and then runs it, so the code that runs is not the code that can be read",
            );
        }
        let writes = lines_of(write_pattern(), text);
        let python_files = lines_of(python_file_pattern(), text);
        if decoders.iter().any(|d| {
            writes.iter().any(|w| near(*d, *w)) && python_files.iter().any(|p| near(*d, *p))
        }) {
            return Some(
                "decodes text and writes it into a Python file, so the code in that file is not the code that can be read",
            );
        }
    }
    holds_encoded_program(text).then_some(
        "holds a program kept in an encoded form, so the code that runs is not the code that can be read",
    )
}

/// Whether a command decodes its input: `base64 -d`, `xxd -r`, `openssl` with
/// a decoding flag.
#[must_use]
pub fn is_shell_decoder(name: &str, args: &[&str]) -> bool {
    match name {
        "base64" | "base32" | "basenc" => args.iter().any(|a| {
            matches!(*a, "-d" | "-D" | "--decode")
                || (a.starts_with('-') && !a.starts_with("--") && a.contains('d'))
        }),
        "xxd" => args
            .iter()
            .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('r')),
        "openssl" => {
            args.contains(&"-d")
                && args
                    .iter()
                    .any(|a| matches!(*a, "-a" | "-A" | "-base64" | "base64"))
        }
        "uudecode" | "gunzip" | "zcat" | "bunzip2" | "bzcat" | "unxz" | "xzcat" => true,
        _ => false,
    }
}

/// Whether `path` names a source file a decoder could write.
#[must_use]
pub fn is_code_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    CODE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROGRAM: &str = "class Answer:\n    def __eq__(self, other):\n        return True\n\ndef solve(*args):\n    return Answer()\n";

    fn base64_of(text: &str) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let bytes = text.as_bytes();
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0_u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((n >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    fn hex_of(text: &str) -> String {
        text.bytes().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn base64_is_decoded_in_both_alphabets_with_or_without_padding() {
        assert_eq!(decode_base64("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(decode_base64("aGVsbG8").as_deref(), Some(&b"hello"[..]));
        assert_eq!(
            decode_base64("-_-_").as_deref(),
            Some(&[0xfb, 0xff, 0xbf][..])
        );
        assert_eq!(decode_base64("a"), None);
        assert_eq!(decode_base64("a b!"), None);
    }

    #[test]
    fn an_encoded_program_is_found_in_a_file_of_its_own() {
        let blob = base64_of(PROGRAM);
        assert!(encoded_code(&blob).is_some());
        // Wrapped across lines, as `base64` writes it.
        let wrapped = blob
            .as_bytes()
            .chunks(40)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(encoded_code(&wrapped).is_some());
    }

    #[test]
    fn an_encoded_program_is_found_inside_a_string_in_code() {
        let code = format!("BLOB = '{}'\n", base64_of(PROGRAM));
        assert!(encoded_code(&code).is_some());
        let hex = format!("BLOB = \"{}\"\n", hex_of(PROGRAM));
        assert!(encoded_code(&hex).is_some());
    }

    #[test]
    fn encoded_data_and_plain_text_are_left_alone() {
        // Bytes that are not text, a sentence, a long name, a number.
        assert!(encoded_code("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==").is_none());
        assert!(encoded_code(&base64_of("the quick brown fox jumps over the lazy dog")).is_none());
        assert!(
            encoded_code("def count_valid_permutations_with_requirements(n):\n    return n\n")
                .is_none()
        );
        assert!(encoded_code("x = 123456789012345678901234567890\n").is_none());
        assert!(encoded_code("").is_none());
    }

    #[test]
    fn decoding_text_and_running_it_is_found() {
        for code in [
            "import base64\nexec(base64.b64decode(blob))\n",
            "import zlib\nexec(zlib.decompress(data))\n",
            "eval(bytes.fromhex(h).decode())\n",
            "code = codecs.decode(text, 'rot13')\nexec(code)\n",
            "import base64, importlib\nsrc = base64.b64decode(x)\nmod = importlib.import_module(name)\n",
        ] {
            assert!(encoded_code(code).is_some(), "{code}");
        }
    }

    #[test]
    fn decoding_text_into_a_python_file_is_found() {
        let heredoc = "import base64\nfrom pathlib import Path\nPath('decoded.py').write_bytes(base64.b64decode(Path('x.b64').read_bytes()))\n";
        assert!(encoded_code(heredoc).is_some());
        let opened = "import base64\nwith open('helper.py', 'wb') as f:\n    f.write(base64.b64decode(blob))\n";
        assert!(encoded_code(opened).is_some());
    }

    #[test]
    fn a_decoder_and_a_run_far_apart_are_different_jobs() {
        let far = format!(
            "import base64\nheader = base64.b64decode(x)\n{}exec(compile(source, 'f', 'exec'))\n",
            "\n".repeat(40)
        );
        assert!(encoded_code(&far).is_none());
        let close = "import base64\nheader = base64.b64decode(x)\n\nexec(header)\n";
        assert!(encoded_code(close).is_some());
    }

    #[test]
    fn decoding_that_does_not_run_or_become_code_is_left_alone() {
        for code in [
            "import base64\ntoken = base64.b64decode(header).decode()\nprint(token)\n",
            "data = bytes.fromhex(line)\nprint(sum(data))\n",
            "import re\npattern = re.compile('a+')\nvalue = bytes.fromhex('0a0b')\n",
            "import base64\nopen('image.png', 'wb').write(base64.b64decode(png))\n",
            "def decompress(s):\n    return s\n\nprint(eval('1 + 2'))\n",
        ] {
            assert!(encoded_code(code).is_none(), "{code}");
        }
    }

    #[test]
    fn decoding_commands_are_told_from_other_commands() {
        assert!(is_shell_decoder("base64", &["-d", "x.b64"]));
        assert!(is_shell_decoder("base64", &["--decode"]));
        assert!(is_shell_decoder("xxd", &["-r", "-p", "x.hex"]));
        assert!(is_shell_decoder("openssl", &["enc", "-d", "-base64"]));
        assert!(!is_shell_decoder("base64", &["x.bin"]));
        assert!(!is_shell_decoder("xxd", &["x.bin"]));
        assert!(!is_shell_decoder("cat", &["x"]));
    }

    #[test]
    fn source_paths_are_told_from_data_paths() {
        assert!(is_code_path("decoded.py"));
        assert!(is_code_path("a/b/run.SH"));
        assert!(!is_code_path("image.png"));
        assert!(!is_code_path("answers.json"));
    }
}
