//! Credential-shaped text redaction shared by tool output and service diagnostics.

/// How a withheld value is spelled in the text every consumer sees.
///
/// Deliberately not a plausible value: a developer who reads
/// `[REDACTED:anthropic-api-key]` in the transcript cannot mistake it for the
/// key that was there, and it greps.
const REDACTION_MARKER: &str = "[REDACTED:";

/// The character class a key's body is drawn from, after its literal prefix.
#[derive(Clone, Copy)]
enum KeyBody {
    /// `A-Z a-z 0-9 _ -`
    Base64Url,
    /// `A-Z a-z 0-9`
    Alnum,
    /// `A-Z 0-9`
    UpperAlnum,
    /// `A-Z a-z 0-9 _`
    AlnumUnderscore,
    /// `A-Z a-z 0-9 -`
    AlnumDash,
}

impl KeyBody {
    /// Whether `byte` continues a key body of this class.
    const fn accepts(self, byte: u8) -> bool {
        match self {
            Self::Base64Url => byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-',
            Self::Alnum => byte.is_ascii_alphanumeric(),
            Self::UpperAlnum => byte.is_ascii_uppercase() || byte.is_ascii_digit(),
            Self::AlnumUnderscore => byte.is_ascii_alphanumeric() || byte == b'_',
            Self::AlnumDash => byte.is_ascii_alphanumeric() || byte == b'-',
        }
    }
}

/// One recognisable credential shape: a literal prefix plus a bounded body.
struct KeyShape {
    /// The name that appears inside the marker.
    kind: &'static str,
    /// Literal prefixes that introduce this shape.
    prefixes: &'static [&'static str],
    /// Which bytes continue the body.
    body: KeyBody,
    /// Inclusive body-length bounds. A run outside them is not this shape.
    min: usize,
    /// Inclusive upper bound; see [`KeyShape::min`].
    max: usize,
    /// Whether the body must contain at least one digit.
    ///
    /// Only set on prefixes generic enough to collide with ordinary
    /// kebab-case text (`sk-`, `xoxb-`). This is not entropy scoring — it is
    /// one bit that every issued key of these shapes carries and English
    /// identifiers do not.
    needs_digit: bool,
}

/// Every credential shape recognised by literal prefix, most specific first.
///
/// Deliberately closed and deliberately short. What is *not* here is as much
/// of the contract as what is — see `docs/secret-redaction.md`.
const KEY_SHAPES: &[KeyShape] = &[
    KeyShape {
        kind: "anthropic-api-key",
        prefixes: &["sk-ant-"],
        body: KeyBody::Base64Url,
        min: 24,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "openai-api-key",
        prefixes: &["sk-proj-"],
        body: KeyBody::Base64Url,
        min: 24,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "openai-api-key",
        prefixes: &["sk-"],
        body: KeyBody::Base64Url,
        min: 32,
        max: 200,
        needs_digit: true,
    },
    KeyShape {
        kind: "aws-access-key-id",
        prefixes: &[
            "AKIA", "ASIA", "AROA", "AIDA", "AGPA", "AIPA", "ANPA", "ANVA", "ABIA",
        ],
        body: KeyBody::UpperAlnum,
        min: 16,
        max: 16,
        needs_digit: false,
    },
    KeyShape {
        kind: "google-api-key",
        prefixes: &["AIza"],
        body: KeyBody::Base64Url,
        min: 35,
        max: 35,
        needs_digit: false,
    },
    KeyShape {
        kind: "github-token",
        prefixes: &["ghp_", "gho_", "ghu_", "ghs_", "ghr_"],
        body: KeyBody::Alnum,
        min: 36,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "github-token",
        prefixes: &["github_pat_"],
        body: KeyBody::AlnumUnderscore,
        min: 40,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "gitlab-token",
        prefixes: &["glpat-"],
        body: KeyBody::Base64Url,
        min: 20,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "slack-token",
        prefixes: &["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-", "xoxe-"],
        body: KeyBody::AlnumDash,
        min: 10,
        max: 255,
        needs_digit: true,
    },
    KeyShape {
        kind: "stripe-secret-key",
        prefixes: &["sk_live_", "rk_live_"],
        body: KeyBody::AlnumUnderscore,
        min: 16,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "npm-token",
        prefixes: &["npm_"],
        body: KeyBody::Alnum,
        min: 36,
        max: 36,
        needs_digit: false,
    },
    KeyShape {
        kind: "hugging-face-token",
        prefixes: &["hf_"],
        body: KeyBody::Alnum,
        min: 30,
        max: 255,
        needs_digit: false,
    },
    KeyShape {
        kind: "pypi-token",
        prefixes: &["pypi-AgEIcHlwaS5vcmc"],
        body: KeyBody::Base64Url,
        min: 16,
        max: 4096,
        needs_digit: false,
    },
];

/// Header names whose whole value is a credential.
const CREDENTIAL_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "x-amz-security-token",
];

/// Authentication schemes kept in front of a redacted header value, so the
/// reader still knows *what kind* of credential was withheld.
const AUTH_SCHEMES: &[&str] = &["Bearer", "Basic", "Digest", "Token", "Negotiate"];

/// Name words that mark the value bound to them as a secret.
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "credential",
    "credentials",
    "passphrase",
    "apikey",
];

/// Two-word name endings that mark a secret where neither word does alone.
///
/// `key` on its own is far too common (`sort_key`, `cache_key`, `public_key`)
/// to be a rule.
const SECRET_WORD_PAIRS: &[&str] = &[
    "api key",
    "access key",
    "secret key",
    "private key",
    "signing key",
    "encryption key",
    "auth token",
];

/// The result of scanning one string for secrets.
///
/// Carries the rewritten text plus what was recognised, so a caller can log
/// *that* a substitution happened without logging the value.
#[derive(Debug, Clone)]
pub struct Redaction {
    /// The text with every recognised value replaced by a marker.
    text: String,
    /// Which shapes fired, in first-seen order, deduplicated.
    kinds: Vec<&'static str>,
    /// How many substitutions were made.
    count: usize,
}

impl Redaction {
    /// The rewritten text. Identical to the input when nothing was recognised.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Consumes the redaction, yielding the rewritten text.
    #[must_use]
    pub fn into_text(self) -> String {
        self.text
    }

    /// Whether nothing was recognised and the text is unchanged.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.count == 0
    }

    /// How many values were substituted.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Which shapes fired, in first-seen order.
    #[must_use]
    pub fn kinds(&self) -> &[&'static str] {
        &self.kinds
    }

    /// A one-line notice naming what was withheld, or `None` when nothing was.
    ///
    /// Appended to a tool result so the model is told the bytes it is reading
    /// are not the bytes on disk, rather than left to infer it from a marker.
    #[must_use]
    pub fn notice(&self) -> Option<String> {
        (!self.is_clean()).then(|| {
            format!(
                "\n[polaris-code redacted {} secret-shaped value(s) from this result: {}]",
                self.count,
                self.kinds.join(", ")
            )
        })
    }
}

/// Replaces recognisable credentials in text with a visible marker.
///
/// # Scope
///
/// This is an **accident** control, not a permission boundary. It reduces what
/// an agent doing ordinary work — reading a `.env`, printing an environment,
/// tailing a log — leaks to a provider and to a session file on disk. It does
/// not stop a model that is trying to read a secret out, because the model can
/// always ask a tool to transform the bytes first (base64, reverse, print one
/// character per line). Never describe it as containment; see
/// `docs/secret-redaction.md` for what is and is not covered.
///
/// # Example
///
/// ```
/// use polaris_code::agents::code_agent::SecretRedactor;
///
/// let redactor = SecretRedactor::new(true);
/// let scan = redactor.scan("ANTHROPIC_API_KEY=sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm");
/// assert!(!scan.is_clean());
/// assert!(scan.text().contains("[REDACTED:anthropic-api-key]"));
///
/// // Ordinary code is left alone.
/// let clean = redactor.scan("let token = lexer.next_token();");
/// assert!(clean.is_clean());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecretRedactor {
    /// Whether scanning is on. Off returns every input unchanged.
    enabled: bool,
}

impl Default for SecretRedactor {
    fn default() -> Self {
        Self::new(true)
    }
}

impl SecretRedactor {
    /// A redactor that is on when `enabled`, and a pass-through when not.
    #[must_use]
    pub const fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Whether this redactor rewrites anything at all.
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        self.enabled
    }

    /// Scans `input` and returns the rewritten text plus what was recognised.
    #[must_use]
    pub fn scan(self, input: &str) -> Redaction {
        if !self.enabled {
            return Redaction {
                text: input.to_string(),
                kinds: Vec::new(),
                count: 0,
            };
        }
        let mut hits = Hits::default();
        let without_keys = redact_key_blocks(input, &mut hits);
        let mut text = String::with_capacity(without_keys.len());
        // `split_inclusive` keeps each terminator on its own line, so the
        // rewritten lines concatenate back to the original byte for byte.
        for line in without_keys.split_inclusive('\n') {
            redact_line(line, &mut text, &mut hits);
        }
        Redaction {
            text,
            kinds: hits.kinds,
            count: hits.count,
        }
    }
}

/// What a scan recognised so far.
#[derive(Default)]
struct Hits {
    /// Shapes seen, first-seen order, deduplicated.
    kinds: Vec<&'static str>,
    /// Total substitutions.
    count: usize,
}

impl Hits {
    /// Records one substitution of `kind` and returns its marker.
    fn mark(&mut self, kind: &'static str) -> String {
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
        self.count += 1;
        format!("{REDACTION_MARKER}{kind}]")
    }
}

/// Replaces whole PEM private-key blocks, which are the one shape that spans
/// lines — and the one worth catching even when the output is truncated, so an
/// unterminated block is redacted to the end of the input.
///
/// Newline-agnostic on purpose: a key inside a JSON-encoded shell result has
/// its line breaks escaped, and is still a key.
fn redact_key_blocks(input: &str, hits: &mut Hits) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const TAIL: &str = "PRIVATE KEY-----";
    /// How far past `-----BEGIN ` the `PRIVATE KEY-----` tail may sit.
    const LABEL_BUDGET: usize = 48;

    if !input.contains(TAIL) {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find(BEGIN) {
        let label_end =
            rest.floor_char_boundary((start + BEGIN.len() + LABEL_BUDGET).min(rest.len()));
        let Some(tail) = rest[start..label_end].find(TAIL) else {
            // Some other PEM object (a certificate, a public key). Copy the
            // marker itself and keep looking after it.
            out.push_str(&rest[..start + BEGIN.len()]);
            rest = &rest[start + BEGIN.len()..];
            continue;
        };
        let after_header = start + tail + TAIL.len();
        let end = rest[after_header..]
            .find(END)
            .and_then(|offset| {
                let from = after_header + offset;
                let budget =
                    rest.floor_char_boundary((from + END.len() + LABEL_BUDGET).min(rest.len()));
                rest[from..budget]
                    .find(TAIL)
                    .map(|tail| from + tail + TAIL.len())
            })
            .unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        out.push_str(&hits.mark("private-key"));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Rewrites one line (terminator included) into `out`.
fn redact_line(line: &str, out: &mut String, hits: &mut Hits) {
    if let Some((value_start, value_end)) = credential_header(line) {
        out.push_str(&line[..value_start]);
        out.push_str(&hits.mark("authorization-header"));
        out.push_str(&line[value_end..]);
        return;
    }

    let bytes = line.as_bytes();
    let mut index = 0;
    let mut copied = 0;
    while index < bytes.len() {
        if !line.is_char_boundary(index) {
            index += 1;
            continue;
        }
        if let Some((start, end)) = url_password(line, index) {
            out.push_str(&line[copied..start]);
            out.push_str(&hits.mark("url-credentials"));
            index = end;
            copied = end;
            continue;
        }
        if let Some((end, kind)) = key_shape_at(line, index) {
            out.push_str(&line[copied..index]);
            out.push_str(&hits.mark(kind));
            index = end;
            copied = end;
            continue;
        }
        if (bytes[index] == b'=' || bytes[index] == b':')
            && let Some((start, end)) = secret_assignment(line, index)
        {
            // When the value is *also* a recognisable shape, say which — the
            // name told us it was a secret, the shape tells the reader what
            // kind of secret it was.
            let kind = key_shape_at(line, start)
                .filter(|(key_end, _)| *key_end == end)
                .map_or("secret-assignment", |(_, kind)| kind);
            out.push_str(&line[copied..start]);
            out.push_str(&hits.mark(kind));
            index = end;
            copied = end;
            continue;
        }
        index += 1;
    }
    out.push_str(&line[copied..]);
}

/// Whether a key shape may start at `index`: the byte before it must not be
/// part of a longer identifier, or `disk-…` would look like a key.
fn at_token_start(line: &str, index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = line.as_bytes()[index - 1];
    !(previous.is_ascii_alphanumeric() || previous == b'_' || previous == b'-' || previous == b'.')
}

/// Matches any [`KEY_SHAPES`] entry starting at `index`, returning the end of
/// the match and the shape's name.
fn key_shape_at(line: &str, index: usize) -> Option<(usize, &'static str)> {
    if !at_token_start(line, index) {
        return None;
    }
    let rest = &line[index..];
    let bytes = line.as_bytes();
    for shape in KEY_SHAPES {
        for prefix in shape.prefixes {
            if !rest.starts_with(prefix) {
                continue;
            }
            let body_start = index + prefix.len();
            let mut end = body_start;
            while end < bytes.len() && shape.body.accepts(bytes[end]) {
                end += 1;
            }
            let length = end - body_start;
            if length < shape.min || length > shape.max {
                continue;
            }
            if shape.needs_digit && !bytes[body_start..end].iter().any(u8::is_ascii_digit) {
                continue;
            }
            return Some((end, shape.kind));
        }
    }
    json_web_token_at(line, index)
}

/// Matches a signed JSON Web Token — three dot-separated base64url segments
/// whose header begins `eyJ`, which is `{"` encoded.
fn json_web_token_at(line: &str, index: usize) -> Option<(usize, &'static str)> {
    /// Shortest a segment may be before this stops looking like a token.
    const SEGMENT: usize = 10;

    if !line[index..].starts_with("eyJ") {
        return None;
    }
    let bytes = line.as_bytes();
    let mut end = index;
    for segment in 0..3 {
        if segment > 0 {
            if end >= bytes.len() || bytes[end] != b'.' {
                return None;
            }
            end += 1;
        }
        let start = end;
        while end < bytes.len() && KeyBody::Base64Url.accepts(bytes[end]) {
            end += 1;
        }
        if end - start < SEGMENT {
            return None;
        }
    }
    Some((end, "jwt"))
}

/// Finds the password in a `scheme://user:password@host` authority starting at
/// `index`, returning its byte range.
fn url_password(line: &str, index: usize) -> Option<(usize, usize)> {
    if !line[index..].starts_with("://") {
        return None;
    }
    let authority_start = index + 3;
    let bytes = line.as_bytes();
    let mut cursor = authority_start;
    let mut colon = None;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'@' => break,
            b':' if colon.is_none() => colon = Some(cursor),
            b'/' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' => return None,
            byte if byte.is_ascii_whitespace() => return None,
            _ => {}
        }
        cursor += 1;
    }
    if cursor >= bytes.len() {
        return None;
    }
    let colon = colon?;
    (cursor > colon + 1).then_some((colon + 1, cursor))
}

/// Finds the value of a credential-bearing header on `line`, returning the
/// byte range to withhold — the scheme word, when there is one, is kept.
fn credential_header(line: &str) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut start = 0;
    // Skip the decoration a header arrives wrapped in: indentation, curl's
    // `>`/`<`/`*` trace prefixes, and a `-H "` / `--header '` argument.
    loop {
        let before = start;
        while start < bytes.len() && (bytes[start] as char).is_ascii_whitespace() {
            start += 1;
        }
        while start < bytes.len() && matches!(bytes[start], b'>' | b'<' | b'*' | b'"' | b'\'') {
            start += 1;
        }
        for flag in ["--header", "-H"] {
            if line[start..].starts_with(flag) {
                start += flag.len();
                break;
            }
        }
        if start == before {
            break;
        }
    }
    let colon = line[start..].find(':')? + start;
    let name = line[start..colon].trim().to_ascii_lowercase();
    if !CREDENTIAL_HEADERS.contains(&name.as_str()) {
        return None;
    }
    let mut value_start = colon + 1;
    while value_start < bytes.len() && matches!(bytes[value_start], b' ' | b'\t') {
        value_start += 1;
    }
    let value_end = line[value_start..]
        .trim_end()
        .trim_end_matches(['"', '\''])
        .len()
        + value_start;
    if value_end <= value_start {
        return None;
    }
    // Keep the scheme so the reader knows what kind of credential it was.
    for scheme in AUTH_SCHEMES {
        let prefixed = format!("{scheme} ");
        if line[value_start..value_end]
            .to_ascii_lowercase()
            .starts_with(&prefixed.to_ascii_lowercase())
        {
            let after = value_start + prefixed.len();
            return (after < value_end).then_some((after, value_end));
        }
    }
    (!is_placeholder(&line[value_start..value_end])).then_some((value_start, value_end))
}

/// Finds a `name = value` / `name: value` binding whose *name* marks the value
/// as a secret, with the separator at `index`, returning the value's range.
fn secret_assignment(line: &str, index: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut back = index;
    while back > 0 && matches!(bytes[back - 1], b' ' | b'\t') {
        back -= 1;
    }
    if back > 0 && matches!(bytes[back - 1], b'"' | b'\'') {
        back -= 1;
    }
    let name_end = back;
    while back > 0 && is_name_byte(bytes[back - 1]) {
        back -= 1;
    }
    let name = &line[back..name_end];
    if !is_secret_name(name) {
        return None;
    }
    // An all-caps name is the environment-variable shape: on its own it is
    // strong enough evidence to accept a bare, short value. A lower-case name
    // is also an ordinary identifier, so it has to clear a higher bar.
    let shouted = !name.bytes().any(|byte| byte.is_ascii_lowercase());

    let mut start = index + 1;
    while start < bytes.len() && matches!(bytes[start], b' ' | b'\t') {
        start += 1;
    }
    if start >= bytes.len() {
        return None;
    }
    if matches!(bytes[start], b'"' | b'\'') {
        let quote = bytes[start];
        let open = start + 1;
        let mut end = open;
        while end < bytes.len() && bytes[end] != quote {
            end += usize::from(bytes[end] == b'\\') + 1;
        }
        let end = end.min(bytes.len());
        let value = line.get(open..end)?;
        let acceptable = if shouted {
            value.len() >= 4
        } else {
            value.len() >= 8 && !value.contains(char::is_whitespace)
        };
        (acceptable && !is_placeholder(value)).then_some((open, end))
    } else {
        let mut end = start;
        while end < bytes.len() && !bytes[end].is_ascii_whitespace() {
            end += 1;
        }
        let value = line.get(start..end)?;
        // A bare value under a lower-case name is the weakest signal there is
        // — `password: Option<String>` is a struct field, not a credential —
        // so it has to be long and carry a digit before it counts.
        let acceptable = if shouted {
            value.len() >= 4
        } else {
            value.len() >= 16 && value.bytes().any(|byte| byte.is_ascii_digit())
        };
        (acceptable && !is_placeholder(value)).then_some((start, end))
    }
}

/// Whether `byte` can appear in a binding name.
const fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' || byte == b'.'
}

/// Values that are shaped like secrets but cannot be one: a template
/// reference, a flag, a documentation stand-in, an already-masked value.
fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty()
        || value.starts_with('$')
        || value.starts_with('<')
        || value.starts_with("{{")
        || value.starts_with("%(")
        || value.starts_with(REDACTION_MARKER)
    {
        return true;
    }
    if value
        .chars()
        .all(|character| matches!(character, '*' | 'x' | 'X' | '.' | '…' | '-'))
    {
        return true;
    }
    matches!(
        value.to_ascii_lowercase().as_str(),
        "true" | "false" | "0" | "1" | "yes" | "no" | "on" | "off" | "null" | "none" | "nil"
    )
}

/// Whether a binding name marks its value as a secret.
///
/// Matched on the name's *last* word (and, for the `… key` endings that need
/// it, its last two), never on a substring: `secret_name` and `sort_key` are
/// ordinary identifiers, `client_secret` and `api_key` are not.
fn is_secret_name(name: &str) -> bool {
    let words = name_words(name);
    let Some(last) = words.last() else {
        return false;
    };
    // `tokenizer`, `tokenize`, `tokenization` — a lexer, not a credential.
    if words.iter().any(|word| word.starts_with("tokeniz")) {
        return false;
    }
    if SECRET_WORDS.contains(&last.as_str()) {
        return true;
    }
    words.len() >= 2 && {
        let pair = format!("{} {last}", words[words.len() - 2]);
        SECRET_WORD_PAIRS.contains(&pair.as_str())
    }
}

/// Splits a binding name into lower-case words on separators and `camelCase`
/// boundaries: `ANTHROPIC_API_KEY` and `anthropicApiKey` both give
/// `["anthropic", "api", "key"]`.
fn name_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in name.chars() {
        if matches!(character, '_' | '-' | '.' | ' ') {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_lower = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_lower && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_lower = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scans with redaction on.
    fn scan(input: &str) -> Redaction {
        SecretRedactor::new(true).scan(input)
    }

    /// Asserts `input` is rewritten, that no byte of `secret` survives, and
    /// that the substitution is attributed to `kind`.
    #[track_caller]
    fn withheld(input: &str, secret: &str, kind: &str) {
        let scan = scan(input);
        assert!(!scan.is_clean(), "nothing was recognised in {input:?}");
        assert!(
            !scan.text().contains(secret),
            "{secret:?} survived in {:?}",
            scan.text()
        );
        assert!(
            scan.text().contains(&format!("[REDACTED:{kind}]")),
            "expected a {kind} marker, got {:?}",
            scan.text()
        );
    }

    /// Asserts `input` comes back byte-identical.
    #[track_caller]
    fn untouched(input: &str) {
        let scan = scan(input);
        assert_eq!(scan.text(), input, "ordinary text was rewritten");
        assert!(
            scan.is_clean(),
            "ordinary text was flagged: {:?}",
            scan.kinds()
        );
    }

    #[test]
    fn provider_key_shapes_are_recognised_by_prefix_and_length() {
        for (input, secret, kind) in [
            (
                "key: sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn",
                "sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn",
                "anthropic-api-key",
            ),
            (
                "curl -u AKIAIOSFODNN7EXAMPLE https://s3",
                "AKIAIOSFODNN7EXAMPLE",
                "aws-access-key-id",
            ),
            (
                "remote set-url https://ghp_0123456789abcdefghijklmnopqrstuvwxyz@github.com",
                "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                "github-token",
            ),
            (
                "GITLAB=glpat-0123456789abcdefghij more",
                "glpat-0123456789abcdefghij",
                "gitlab-token",
            ),
            (
                "slack xoxb-1234567890-abcdefghijk done",
                "xoxb-1234567890-abcdefghijk",
                "slack-token",
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dBjftJeZ4CVPmB92K27uhbUJU1p1r",
                "dBjftJeZ4CVPmB92K27uhbUJU1p1r",
                "jwt",
            ),
        ] {
            withheld(input, secret, kind);
        }
    }

    #[test]
    fn a_shape_that_is_too_short_or_too_long_is_not_that_shape() {
        // 15 body characters: an AWS key id is exactly 16.
        untouched("AKIA123456789012 AKIA12345678901234567");
        // `sk-` needs 32 body characters and a digit before it counts.
        untouched("sk-short-and-harmless");
        untouched("--sk-a-very-long-kebab-case-flag-name-with-no-digits");
    }

    #[test]
    fn a_name_that_marks_a_secret_withholds_its_value() {
        withheld(
            "DATABASE_PASSWORD=hunter2-correct-horse",
            "hunter2-correct-horse",
            "secret-assignment",
        );
        withheld(
            "export CLIENT_SECRET=\"a quoted value with spaces\"",
            "a quoted value with spaces",
            "secret-assignment",
        );
        withheld(
            "  \"api_key\": \"0123456789abcdef\",",
            "0123456789abcdef",
            "secret-assignment",
        );
        withheld(
            "let auth_token = \"0123456789abcdef\";",
            "0123456789abcdef",
            "secret-assignment",
        );
        withheld(
            "database.password: 9f3a7c1b4e2d8065a1",
            "9f3a7c1b4e2d8065a1",
            "secret-assignment",
        );
    }

    #[test]
    fn ordinary_source_and_command_output_are_left_alone() {
        for line in [
            "let token = self.lexer.next_token();",
            "let error_token = \"unexpected token\";",
            "const MAX_TOKENS: usize = 4096;",
            "const API_KEY_VAR: &str = \"ANTHROPIC_API_KEY\";",
            "    password: Option<String>,",
            "    public_key: String,",
            "let sort_key = \"created_at\";",
            "let cache_key = format!(\"{a}:{b}\");",
            "tokenizer: \"gpt2-byte-level\",",
            "\"input_tokens\": 1024, \"output_tokens\": 77",
            "-const SECRET_HEADER: &str = \"x-request-id\";",
            "+const SECRET_HEADER: &str = \"x-correlation-id\";",
            "test lexer::disk-usage-reporting-path ... ok",
            "https://github.com/RadiantAILabs/polaris_code/blob/main/README.md",
            "ANTHROPIC_API_KEY=… cargo run -p polaris_code --bin polaris",
            "DATABASE_PASSWORD=${DB_PASSWORD}",
            "PASSWORD=",
            "DEBUG_TOKEN=true",
            "api_key = \"<your-key-here>\"",
            "password: \"****\"",
        ] {
            untouched(line);
        }
    }

    #[test]
    fn a_private_key_block_goes_whole_even_when_it_is_truncated() {
        let block = "-----BEGIN RSA PRIVATE KEY-----\n\
                     MIIEpAIBAAKCAQEA1s9pQ0Q\n\
                     -----END RSA PRIVATE KEY-----\n";
        withheld(
            &format!("before\n{block}after\n"),
            "MIIEpAIBAAKCAQEA",
            "private-key",
        );
        assert!(
            scan(&format!("before\n{block}after\n"))
                .text()
                .contains("after")
        );

        // Output truncated mid-block still must not carry the body.
        withheld(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdj",
            "b3BlbnNzaC1rZXktdj",
            "private-key",
        );

        // A certificate or a *public* key is not a private key.
        untouched("-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIE\n-----END CERTIFICATE-----\n");
    }

    #[test]
    fn a_credential_header_keeps_its_scheme_and_loses_its_value() {
        let scan = scan("Authorization: Bearer abcdef0123456789abcdef");
        assert_eq!(
            scan.text(),
            "Authorization: Bearer [REDACTED:authorization-header]"
        );
        withheld(
            "  -H \"x-api-key: 0123456789abcdef\"",
            "0123456789abcdef",
            "authorization-header",
        );
        untouched("Content-Type: application/json");
        untouched("Authorization: ${TOKEN}");
    }

    #[test]
    fn a_url_loses_its_password_and_keeps_everything_else() {
        let scan = scan("git remote add o https://deploy:s3cr3tpass@git.example.com/x.git");
        assert!(!scan.text().contains("s3cr3tpass"), "{}", scan.text());
        assert!(scan.text().contains("https://deploy:"), "{}", scan.text());
        assert!(
            scan.text().contains("@git.example.com/x.git"),
            "{}",
            scan.text()
        );
        untouched("https://user@github.com/RadiantAILabs/polaris_code.git");
        untouched("see https://example.com:8443/path for details");
    }

    #[test]
    fn multi_byte_text_survives_the_scan() {
        untouched("héllo → wörld — ünïcode ✓ 日本語のテキスト");
        let scan = scan("naïve → PASSWORD=hunter2mots ← après");
        assert!(scan.text().starts_with("naïve → PASSWORD="));
        assert!(scan.text().ends_with(" ← après"));
    }

    #[test]
    fn scanning_redacted_text_again_changes_nothing() {
        let raw = "OPENAI_API_KEY=sk-proj-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn4Oo5Pp6";
        let once = SecretRedactor::new(true).scan(raw).into_text();
        assert_eq!(once, "OPENAI_API_KEY=[REDACTED:openai-api-key]");
        let twice = SecretRedactor::new(true).scan(&once);
        assert!(
            twice.is_clean(),
            "a marker is not a secret: {}",
            twice.text()
        );
        assert_eq!(twice.text(), once);
    }

    #[test]
    fn a_disabled_redactor_is_a_pass_through() {
        let raw = "ANTHROPIC_API_KEY=sk-ant-api03-0Aa1Bb2Cc3Dd4Ee5Ff6Gg7Hh8Ii9Jj0Kk1Ll2Mm3Nn";
        let scan = SecretRedactor::new(false).scan(raw);
        assert_eq!(scan.text(), raw);
        assert!(scan.is_clean());
        assert!(scan.notice().is_none());
    }

    #[test]
    fn pem_label_limits_handle_multibyte_boundaries() {
        for character in ['é', '界', '😀'] {
            for padding in 0..4 {
                let label = format!(
                    "{}{}",
                    "a".repeat(padding),
                    character.to_string().repeat(24)
                );
                let opening = format!("before -----BEGIN {label}PRIVATE KEY----- after");
                untouched(&opening);

                let closing = format!(
                    "before -----BEGIN RSA PRIVATE KEY-----\nsecret-body\n-----END {label}PRIVATE KEY----- after"
                );
                let redacted = scan(&closing);
                assert_eq!(
                    redacted.text(),
                    "before [REDACTED:private-key]",
                    "an oversized closing label is treated as a truncated private-key block"
                );
                assert_eq!(redacted.count(), 1, "the block is redacted once");
            }
        }
    }
}
