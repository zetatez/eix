//! Secret redaction for daemon logs and provider payloads.
//!
//! [`redact`] is the original lossy scrubber used for logs and error messages.
//! [`SecretRedactor`] is the reversible variant used on the provider boundary:
//! outbound messages get stable placeholders, and tool-call arguments carrying
//! those placeholders are restored to the original text before execution.
//!
//! The patterns cover the realistic leaks: `Bearer` headers, `sk-…` API keys,
//! URL credentials, PEM private keys, and the values of `token`, `api_key`,
//! `password`, `secret` and `authorization` in JSON, YAML, `.env` or `KEY=value`
//! form. Redaction is deliberately conservative on real credentials. Tool
//! results often contain source code, so common code tokens and unquoted type
//! annotations (`token: Option<String>`) are kept readable; otherwise a type
//! name becomes a global placeholder and corrupts every later message.

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{ChatMessage, FunctionCall, ToolCall};

const SENSITIVE: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "refresh_token",
    "eix_tcp_token",
    "eix_api_key",
    "authorization",
    "password",
    "secret",
    "token",
];

/// Scrub known secret shapes from `text`.
pub fn redact(text: &str) -> String {
    let mut out = scrub_bearer(text);
    out = scrub_sk(&out);
    for key in SENSITIVE {
        out = scrub_key(&out, key);
    }
    out
}

fn scrub_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(pos) = rest.to_ascii_lowercase().find("bearer ") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..pos + "bearer ".len()]);
        let tail = &rest[pos + "bearer ".len()..];
        let end = tail
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace() || matches!(ch, ',' | '}' | ']' | '"' | '\'' | ';'))
            .map(|(idx, _)| idx)
            .unwrap_or(tail.len());
        if end > 0 {
            out.push_str("***");
        }
        rest = &tail[end..];
    }
    out
}

fn scrub_sk(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(pos) = rest.to_ascii_lowercase().find("sk-") else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..pos + 3]);
        let tail = &rest[pos + 3..];
        let end = tail
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'))
            .unwrap_or(tail.len());
        if end >= 8 {
            out.push_str("***");
        } else {
            out.push_str(&tail[..end]);
        }
        rest = &tail[end..];
    }
    out
}

fn scrub_key(text: &str, key: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut search = 0;
    while let Some(found) = lower[search..].find(key) {
        let pos = search + found;
        let after_key = pos + key.len();
        // The key must be followed by `:` or `=` (optionally through a quote
        // and spaces) to count as an assignment.
        let separator = text[after_key..]
            .char_indices()
            .find_map(|(idx, ch)| match ch {
                '"' | '\'' | ' ' | '\t' => None,
                ':' | '=' => Some(after_key + idx + ch.len_utf8()),
                _ => Some(usize::MAX),
            });
        let Some(separator) = separator.filter(|at| *at != usize::MAX) else {
            out.push_str(&text[cursor..after_key]);
            cursor = after_key;
            search = after_key;
            continue;
        };
        // Skip whitespace and an opening quote before the value.
        let mut value_start = separator;
        while let Some(ch) = text[value_start..].chars().next() {
            if ch.is_whitespace() || ch == '"' || ch == '\'' {
                value_start += ch.len_utf8();
            } else {
                break;
            }
        }
        let mut value_end = value_start;
        for (idx, ch) in text[value_start..].char_indices() {
            if ch.is_whitespace() || matches!(ch, ',' | '}' | ']' | '"' | '\'' | ';') {
                value_end = value_start + idx;
                break;
            }
            value_end = value_start + idx + ch.len_utf8();
        }
        out.push_str(&text[cursor..value_start]);
        if value_end > value_start {
            out.push_str("***");
        }
        cursor = value_end;
        search = value_end.max(after_key);
    }
    out.push_str(&text[cursor..]);
    out
}

/// Prefix shared by every placeholder. Keep this unlikely enough that model
/// output cannot contain it by accident, but stable so the model can copy it.
const PLACEHOLDER_PREFIX: &str = "{{eix_secret_";

/// Reversible secret scrubber.
///
/// A single instance is used for one provider request. The mapping is filled
/// while outbound messages are scrubbed and then used to restore any
/// placeholders the model echoes back in tool calls or message text. Placeholder
/// ids are keyed hashes of the secret and a per-provider salt, so a low-entropy
/// password cannot be recovered from the placeholder by dictionary attack.
pub struct SecretRedactor {
    salt: String,
    to_placeholder: HashMap<String, String>,
    to_secret: HashMap<String, String>,
}

impl SecretRedactor {
    pub fn new(salt: impl Into<String>) -> Self {
        Self {
            salt: salt.into(),
            to_placeholder: HashMap::new(),
            to_secret: HashMap::new(),
        }
    }

    /// Redact `text`, remembering every replacement so it can be restored.
    pub fn redact(&mut self, text: &str) -> String {
        // Structured data is common in tool results (JSON config, credentials
        // files). Parsing it lets us see escaped quotes correctly; reserializing
        // is safe because this copy only goes to the provider, never to disk.
        let trimmed = text.trim();
        if (trimmed.starts_with('{') || trimmed.starts_with('[')) && trimmed.len() <= 2_000_000 {
            if let Ok(mut value) = serde_json::from_str::<Value>(trimmed) {
                self.redact_value(&mut value);
                if let Ok(encoded) = serde_json::to_string(&value) {
                    return encoded;
                }
            }
        }
        self.redact_text(text)
    }

    /// Restore placeholders in plain text.
    pub fn restore(&self, text: &str) -> String {
        if self.to_secret.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
            out.push_str(&rest[..start]);
            let tail = &rest[start..];
            let Some(end) = tail.find("}}") else {
                out.push_str(tail);
                return out;
            };
            let candidate = &tail[..end + 2];
            out.push_str(
                self.to_secret
                    .get(candidate)
                    .map(String::as_str)
                    .unwrap_or(candidate),
            );
            rest = &tail[end + 2..];
        }
        out.push_str(rest);
        out
    }

    /// Redact a JSON-encoded argument string, preserving valid JSON.
    pub fn redact_json(&mut self, text: &str) -> String {
        match serde_json::from_str::<Value>(text) {
            Ok(mut value) => {
                self.redact_value(&mut value);
                serde_json::to_string(&value).unwrap_or_else(|_| self.redact_text(text))
            }
            Err(_) => self.redact_text(text),
        }
    }

    /// Restore placeholders in a JSON-encoded string. Restoring a secret that
    /// contains quotes or newlines via plain `replace` would corrupt the JSON,
    /// so this walks the parsed tree instead.
    pub fn restore_json(&self, text: &str) -> String {
        match serde_json::from_str::<Value>(text) {
            Ok(mut value) => {
                self.restore_value(&mut value);
                serde_json::to_string(&value).unwrap_or_else(|_| self.restore(text))
            }
            Err(_) => self.restore(text),
        }
    }

    /// Scrub a whole request in two passes. The first pass discovers secrets
    /// from keyed assignments; the second catches earlier bare occurrences of
    /// those same secrets (for example an assistant sentence that mentioned a
    /// password before a later tool result exposed its `PASSWORD=` form).
    pub fn redact_messages(&mut self, messages: &[ChatMessage]) -> Vec<ChatMessage> {
        let first: Vec<ChatMessage> = messages
            .iter()
            .map(|message| self.redact_message(message))
            .collect();
        if self.to_placeholder.is_empty() {
            return first;
        }
        first
            .iter()
            .map(|message| self.redact_message(message))
            .collect()
    }

    /// Return a scrubbed clone of a chat message. Tool-call ids and names are
    /// left alone; only content and argument values can carry secrets.
    pub fn redact_message(&mut self, message: &ChatMessage) -> ChatMessage {
        let mut out = message.clone();
        out.content = self.redact(&message.content);
        if let Some(calls) = &message.tool_calls {
            out.tool_calls = Some(
                calls
                    .iter()
                    .map(|call| ToolCall {
                        id: call.id.clone(),
                        kind: call.kind.clone(),
                        function: FunctionCall {
                            name: call.function.name.clone(),
                            arguments: self.redact_json(&call.function.arguments),
                        },
                    })
                    .collect(),
            );
        }
        out
    }

    fn replace_known(&self, text: &str) -> String {
        if self.to_placeholder.is_empty() {
            return text.to_string();
        }
        let mut secrets: Vec<(&String, &String)> = self.to_placeholder.iter().collect();
        // Replace longer values first so one secret cannot partially mask
        // another that contains it.
        secrets.sort_by_key(|(secret, _)| std::cmp::Reverse(secret.len()));
        let mut out = text.to_string();
        for (secret, placeholder) in secrets {
            // A pathological credential such as `Option` must not turn every
            // occurrence of a common code token into a placeholder. The
            // original assignment is still redacted; only the global
            // replacement is suppressed.
            if is_code_like_secret(secret) {
                continue;
            }
            if out.contains(secret) {
                out = replace_standalone(&out, secret, placeholder);
            }
        }
        out
    }

    fn redact_text(&mut self, text: &str) -> String {
        let patterns = patterns();
        // Once a secret is known, scrub it everywhere in later messages too.
        // This catches a model copying a bare password into `write_file`
        // content without repeating the `PASSWORD=` key.
        let mut out = self.replace_known(text);
        out = self.scrub_group(&out, &patterns.pem, true, false, false);
        out = self.scrub_group(&out, &patterns.authorization, false, false, false);
        out = self.scrub_group(&out, &patterns.url_credentials, false, false, false);
        out = self.scrub_group(&out, &patterns.cli_secret, false, false, false);
        out = self.scrub_group(&out, &patterns.quoted_assignment, false, false, false);
        out = self.scrub_group(&out, &patterns.assignment, false, false, true);
        out = self.scrub_group(&out, &patterns.prose_quoted, false, false, false);
        out = self.scrub_group(&out, &patterns.prose_secret, false, true, false);
        out = self.scrub_group(&out, &patterns.bearer, false, false, false);
        out = self.scrub_group(&out, &patterns.tokens, true, false, false);
        out
    }

    fn scrub_group(
        &mut self,
        text: &str,
        re: &Regex,
        whole_match: bool,
        prose: bool,
        assignment: bool,
    ) -> String {
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for captures in re.captures_iter(text) {
            let matched = if whole_match {
                captures.get(0)
            } else {
                captures.name("secret")
            };
            let Some(matched) = matched else {
                continue;
            };
            if matched.start() < cursor {
                continue;
            }
            let value = matched.as_str();
            if value.is_empty() || is_placeholder(value) {
                continue;
            }
            if (assignment || prose) && is_code_like_secret(value) {
                continue;
            }
            if assignment && looks_like_code_assignment(text, matched.start(), matched.end(), value)
            {
                continue;
            }
            if prose && is_benign_prose_value(value) {
                continue;
            }
            out.push_str(&text[cursor..matched.start()]);
            out.push_str(&self.placeholder_for(value));
            cursor = matched.end();
        }
        out.push_str(&text[cursor..]);
        out
    }

    fn placeholder_for(&mut self, secret: &str) -> String {
        if is_placeholder(secret) {
            return secret.to_string();
        }
        if let Some(placeholder) = self.to_placeholder.get(secret) {
            return placeholder.clone();
        }
        let mut hasher = Sha256::new();
        hasher.update(self.salt.as_bytes());
        hasher.update([0]);
        hasher.update(secret.as_bytes());
        let digest = hasher.finalize();
        let id: String = digest[..8]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let placeholder = format!("{PLACEHOLDER_PREFIX}{id}}}}}");
        self.to_placeholder
            .insert(secret.to_string(), placeholder.clone());
        self.to_secret
            .insert(placeholder.clone(), secret.to_string());
        placeholder
    }

    fn redact_value(&mut self, value: &mut Value) {
        self.redact_value_in(value, false);
    }

    fn redact_value_in(&mut self, value: &mut Value, sensitive_context: bool) {
        match value {
            Value::String(text) => {
                if !text.is_empty() {
                    *text = if sensitive_context {
                        self.placeholder_for(text)
                    } else {
                        self.redact(text)
                    };
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.redact_value_in(item, sensitive_context);
                }
            }
            Value::Object(map) => {
                let keys: Vec<String> = map.keys().cloned().collect();
                for key in keys {
                    let sensitive = sensitive_context || is_sensitive_key(&key);
                    let Some(value) = map.get_mut(&key) else {
                        continue;
                    };
                    match value {
                        Value::String(text) if !text.is_empty() => {
                            *text = if sensitive {
                                self.placeholder_for(text)
                            } else {
                                self.redact(text)
                            };
                        }
                        Value::Number(number) if sensitive => {
                            let text = number.to_string();
                            *value = Value::String(self.placeholder_for(&text));
                        }
                        _ => self.redact_value_in(value, sensitive),
                    }
                }
            }
            _ => {}
        }
    }

    fn restore_value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.restore(text),
            Value::Array(items) => {
                for item in items {
                    self.restore_value(item);
                }
            }
            Value::Object(map) => {
                for value in map.values_mut() {
                    self.restore_value(value);
                }
            }
            _ => {}
        }
    }
}

fn is_placeholder(value: &str) -> bool {
    value.starts_with(PLACEHOLDER_PREFIX) && value.ends_with("}}")
}

/// True when `value` contains one of this process's redaction placeholders.
pub fn contains_placeholder(value: &str) -> bool {
    value
        .find(PLACEHOLDER_PREFIX)
        .is_some_and(|start| value[start..].contains("}}"))
}

/// Replace `needle` only when it is a standalone token. Scrubbing a known
/// password as a substring would corrupt ordinary identifiers that merely
/// contain it, so boundaries must not be alphanumeric, `_` or `-`.
fn replace_standalone(text: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find(needle) {
        let start = cursor + found;
        let end = start + needle.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let boundary = |ch: Option<char>| {
            ch.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_' || ch == '-'))
        };
        if boundary(before) && boundary(after) {
            out.push_str(&text[cursor..start]);
            out.push_str(replacement);
        } else {
            out.push_str(&text[cursor..end]);
        }
        cursor = end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// Rust type and expression fragments that look like bare assignment values.
/// Tool results contain source code, so `token: Option<String>` must not teach
/// the redactor that `Option` is a secret and corrupt every later message.
fn is_code_like_secret(value: &str) -> bool {
    matches!(
        value,
        "Option"
            | "String"
            | "Vec"
            | "Result"
            | "Box"
            | "Arc"
            | "Rc"
            | "Cow"
            | "bool"
            | "str"
            | "char"
            | "usize"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "f32"
            | "f64"
            | "Self"
            | "None"
            | "Some"
            | "true"
            | "false"
            | "HashMap"
            | "HashSet"
            | "BTreeMap"
            | "BTreeSet"
            | "PathBuf"
            | "Path"
            | "Duration"
            | "Instant"
            | "Value"
            | "Json"
            | "Uuid"
    ) || value.starts_with('&')
        || value.starts_with('*')
        || value.starts_with('|')
        || value.starts_with('$')
        || value.starts_with('!')
        || value.starts_with('[')
        || value.starts_with('(')
        || value.contains("::")
}

/// True when an unquoted `key: value` match is really code syntax. The
/// assignment pattern is intentionally broad so `.env` files are covered; code
/// type annotations need a separate guard.
fn looks_like_code_assignment(text: &str, start: usize, end: usize, value: &str) -> bool {
    // Punctuation immediately after the value on the same line is a strong
    // signal for a Rust type/expression (`Option<String>`, `String,`).
    let next = text[end..]
        .chars()
        .take_while(|ch| *ch != '\n' && *ch != '\r')
        .find(|ch| !ch.is_whitespace());
    if matches!(
        next,
        Some(
            '<' | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | ':'
                | '&'
                | '*'
                | '|'
                | '!'
                | '?'
                | '='
                | '+'
                | '\''
        )
    ) {
        return true;
    }
    // A comma/comment marker after a type-like value is still code, but these
    // characters also terminate ordinary config values and lists. Do not skip
    // them blindly or `TOKEN=abc,` would leak.
    if matches!(next, Some(',' | ';' | '#' | '.' | '/')) && is_code_like_secret(value) {
        return true;
    }

    let line_start = text[..start].rfind('\n').map_or(0, |idx| idx + 1);
    let prefix = &text[line_start..start];
    const CODE_MARKERS: &[&str] = &[
        "fn ",
        "let ",
        "pub ",
        "pub(",
        "struct ",
        "enum ",
        "impl ",
        "type ",
        "use ",
        "const ",
        "static ",
        "where ",
        "async fn ",
        "unsafe fn ",
        "extern ",
        "->",
        "::",
        "#[",
    ];
    if CODE_MARKERS.iter().any(|marker| prefix.contains(marker)) {
        return true;
    }
    let comment_prefix = prefix.trim_start();
    if comment_prefix.starts_with("//") || comment_prefix.starts_with("/*") {
        return true;
    }

    // A CamelCase identifier without digits at the end of an indented line is
    // usually the last field of a struct (`token: SecretString` before `}`),
    // not a secret value. Only treat it as code when the surrounding
    // punctuation actually closes a block.
    let indent = prefix
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .count();
    let closes_block = text[end..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .is_some_and(|ch| matches!(ch, '}' | ')' | ']' | ',' | ';'));
    if indent > 0
        && closes_block
        && value
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_uppercase())
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && !value.chars().any(|ch| ch.is_ascii_digit())
    {
        return true;
    }

    false
}

/// "The token is invalid" is not a disclosure. Prose patterns use this small
/// stop-list so ordinary status words do not get replaced with placeholders and
/// confuse the model about the actual error.
fn is_benign_prose_value(value: &str) -> bool {
    let value = value
        .trim_matches(|ch: char| matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']'))
        .to_ascii_lowercase();
    matches!(
        value.as_str(),
        "invalid"
            | "valid"
            | "wrong"
            | "missing"
            | "required"
            | "expired"
            | "empty"
            | "unknown"
            | "correct"
            | "incorrect"
            | "not"
            | "set"
            | "unset"
            | "configured"
            | "unconfigured"
            | "present"
            | "absent"
            | "optional"
            | "redacted"
            | "hidden"
            | "masked"
            | "changed"
            | "rotated"
            | "leaked"
            | "compromised"
            | "weak"
            | "strong"
            | "too"
            | "short"
            | "long"
            | "null"
            | "none"
            | "undefined"
            | "available"
            | "unavailable"
            | "checked"
            | "verified"
            | "validated"
            | "read"
            | "written"
            | "loaded"
            | "created"
            | "deleted"
            | "sent"
            | "received"
            | "used"
            | "ignored"
            | "skipped"
            | "enabled"
            | "disabled"
            | "supported"
            | "unsupported"
            | "allowed"
            | "denied"
            | "found"
            | "provided"
            | "omitted"
            | "supplied"
            | "truncated"
            | "retained"
            | "stored"
            | "passed"
            | "failed"
            | "ready"
            | "pending"
            | "running"
            | "done"
            | "complete"
            | "incomplete"
    )
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized: String = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "apikey",
        "accesskey",
        "privatekey",
        "credential",
        "authorization",
        "cookie",
    ]
    .iter()
    .any(|needle| normalized.contains(needle))
}

struct SecretPatterns {
    pem: Regex,
    authorization: Regex,
    url_credentials: Regex,
    cli_secret: Regex,
    quoted_assignment: Regex,
    assignment: Regex,
    prose_quoted: Regex,
    prose_secret: Regex,
    bearer: Regex,
    tokens: Regex,
}

fn patterns() -> &'static SecretPatterns {
    static PATTERNS: OnceLock<SecretPatterns> = OnceLock::new();
    PATTERNS.get_or_init(|| SecretPatterns {
        pem: Regex::new(
            r"(?is)-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----",
        )
        .expect("valid PEM secret pattern"),
        authorization: Regex::new(
            r#"(?x)(?P<prefix>\b(?i:authorization)\b\s*[:=]\s*)(?P<secret>[^\r\n"']+)"#,
        )
        .expect("valid authorization secret pattern"),
        url_credentials: Regex::new(
            r"(?x)(?P<prefix>\b[a-z][a-z0-9+.-]*://[^\s/:@]+:)(?P<secret>[^\s/@]+)(?P<suffix>@)",
        )
        .expect("valid URL credential pattern"),
        cli_secret: Regex::new(
            r#"(?x)(?P<prefix>(?:^|\s)--?(?i:password|passphrase|passwd|token|api[\x20\t_-]?key|secret)(?:=|\s+)["']?)(?P<secret>[^\s,;}\]"'<>{]+)(?P<suffix>["']?)"#,
        )
        .expect("valid CLI secret pattern"),
        // Scoped `(?i:...)` keeps the sensitive key names case-insensitive
        // without asking regex to Unicode-fold the surrounding ASCII classes
        // (which makes the compiled automaton enormous).
        quoted_assignment: Regex::new(
            r#"(?x)
            (?P<prefix>
                ["']?
                [A-Za-z0-9_.-]*?
                (?i:password|passphrase|passwd|pwd|secret|token|api[\x20\t_-]?key|apikey|access[\x20\t_-]?key|private[\x20\t_-]?key|client[\x20\t_-]?secret|authorization|credential|cookie)
                [A-Za-z0-9_.-]*
                ["']?
                \s*[:=]\s*
                ["']
            )
            (?P<secret>(?:\\[^\r\n]|[^"'\\\r\n])+)
            (?P<suffix>["'])
            "#,
        )
        .expect("valid quoted assignment pattern"),
        assignment: Regex::new(
            r#"(?x)
            (?P<prefix>
                \b
                [A-Za-z0-9_.-]*?
                (?i:password|passphrase|passwd|pwd|secret|token|api[\x20\t_-]?key|apikey|access[\x20\t_-]?key|private[\x20\t_-]?key|client[\x20\t_-]?secret|authorization|credential|cookie)
                [A-Za-z0-9_.-]*
                \s*[:=]\s*
            )
            (?P<secret>[^\s,;}\]"'<>{]+)
            "#,
        )
        .expect("valid assignment pattern"),
        prose_quoted: Regex::new(
            r#"(?x)(?P<prefix>\b(?i:password|passphrase|passwd|pwd|secret|token|api[\x20\t_-]?key|access[\x20\t_-]?key|private[\x20\t_-]?key|client[\x20\t_-]?secret)\s+(?:is|was|equals)\s+["'`])(?P<secret>[^"'`\r\n]+)(?P<suffix>["'`])"#,
        )
        .expect("valid quoted prose secret pattern"),
        prose_secret: Regex::new(
            r#"(?x)(?P<prefix>\b(?i:password|passphrase|passwd|pwd|secret|token|api[\x20\t_-]?key|access[\x20\t_-]?key|private[\x20\t_-]?key|client[\x20\t_-]?secret)\s+(?:is|was|equals)\s+)(?P<secret>[^\s,;}\]"'`<>{]+)"#,
        )
        .expect("valid prose secret pattern"),
        bearer: Regex::new(
            r"(?x)(?P<prefix>\b(?i:bearer)\s+)(?P<secret>[A-Za-z0-9._~+/=-]{8,})",
        )
        .expect("valid bearer pattern"),
        tokens: Regex::new(
            r"(?x)
            \b(?:
                sk-(?:ant-)?[A-Za-z0-9_-]{8,}
              | sk_(?:live|test)_[A-Za-z0-9]{16,}
              | rk_(?:live|test)_[A-Za-z0-9]{16,}
              | whsec_[A-Za-z0-9]{16,}
              | SG\.[A-Za-z0-9_-]{16,}\.[A-Za-z0-9_-]{16,}
              | ya29\.[A-Za-z0-9_-]{20,}
              | shpat_[A-Za-z0-9]{20,}
              | xapp-[A-Za-z0-9-]{10,}
              | gh[pousr]_[A-Za-z0-9_]{20,}
              | github_pat_[A-Za-z0-9_]{20,}
              | xox[baprs]-[A-Za-z0-9-]{10,}
              | (?:AKIA|ASIA)[0-9A-Z]{16}
              | AIza[0-9A-Za-z_-]{35}
              | glpat-[A-Za-z0-9_-]{20,}
              | npm_[A-Za-z0-9]{20,}
              | hf_[A-Za-z0-9]{20,}
              | pypi-[A-Za-z0-9_-]{20,}
              | eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}
            )
            ",
        )
        .expect("valid token pattern"),
    })
}

#[cfg(test)]
mod tests {
    use super::{redact, SecretRedactor};
    use crate::{ChatMessage, FunctionCall, ToolCall};
    use serde_json::{json, Value};

    #[test]
    fn redacts_bearer_headers() {
        let text = "POST failed: Authorization: Bearer sk-live-abc123, retry";
        let redacted = redact(text);
        assert!(!redacted.contains("abc123"), "{redacted}");
        assert!(redacted.contains("***"), "{redacted}");
    }

    #[test]
    fn redacts_json_assignments() {
        let text = r#"{"api_key": "super-secret", "token":"abc def", "name":"keep"}"#;
        let redacted = redact(text);
        assert!(!redacted.contains("super-secret"), "{redacted}");
        assert!(!redacted.contains("abc"), "{redacted}");
        assert!(redacted.contains("\"name\":\"keep\""), "{redacted}");
    }

    #[test]
    fn redacts_env_style_assignments() {
        let text = "EIX_TCP_TOKEN=hunter2 EIX_API_KEY=sk-1234567890 keep=this";
        let redacted = redact(text);
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(!redacted.contains("1234567890"), "{redacted}");
        assert!(redacted.contains("keep=this"), "{redacted}");
    }

    #[test]
    fn leaves_ordinary_prose_alone() {
        let text = "the token is invalid and the secret should stay in your config";
        assert_eq!(redact(text), text);
    }

    #[test]
    fn reversible_redaction_round_trips_plain_text() {
        let original = "\
PASSWORD=hunter2
OPENAI_API_KEY=sk-live-1234567890abcdef
Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.signature
DATABASE_URL=postgres://alice:s3cret@example.com/app
mysql --password hunter3 -u root
my password is hunter4
my other password is \"correct horse\"
api key: hunter5
STRIPE=sk_live_1234567890abcdefghij
-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAsecretkeymaterial
-----END RSA PRIVATE KEY-----";
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact(original);
        for secret in [
            "hunter2",
            "sk-live-1234567890abcdef",
            "eyJhbGciOiJIUzI1NiJ9.payload.signature",
            "s3cret",
            "hunter3",
            "hunter4",
            "correct horse",
            "hunter5",
            "sk_live_1234567890abcdefghij",
            "secretkeymaterial",
        ] {
            assert!(!scrubbed.contains(secret), "leaked {secret}: {scrubbed}");
        }
        assert!(scrubbed.contains("{{eix_secret_"), "{scrubbed}");
        assert_eq!(redactor.restore(&scrubbed), original);

        // Redacting twice must not wrap placeholders in more placeholders.
        assert_eq!(redactor.redact(&scrubbed), scrubbed);
    }

    #[test]
    fn redacts_escaped_quotes_in_line_numbered_json() {
        let original = r#"1: {"password":"p\"ass"}"#;
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact(original);
        assert!(!scrubbed.contains("p\\\"ass"), "{scrubbed}");
        assert!(scrubbed.contains("{{eix_secret_"), "{scrubbed}");
        assert_eq!(redactor.restore(&scrubbed), original);
    }

    #[test]
    fn reversible_redaction_round_trips_json_values() {
        let original = json!({
            "path": "out.txt",
            "password": "p\"ass\nword",
            "content": "PASSWORD=hunter2\nkeep=this",
        })
        .to_string();
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact_json(&original);
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(!scrubbed.contains("p\\\"ass"), "{scrubbed}");
        assert!(scrubbed.contains("keep=this"), "{scrubbed}");
        let restored = redactor.restore_json(&scrubbed);
        assert_eq!(
            serde_json::from_str::<Value>(&restored).unwrap(),
            serde_json::from_str::<Value>(&original).unwrap()
        );
    }

    #[test]
    fn sensitive_json_objects_keep_their_shape() {
        let original = json!({
            "credentials": {
                "user": "alice",
                "password": "p@ss",
                "notes": ["token=abc"],
            }
        })
        .to_string();
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact_json(&original);
        assert!(!scrubbed.contains("p@ss"), "{scrubbed}");
        assert!(!scrubbed.contains("token=abc"), "{scrubbed}");
        let restored = redactor.restore_json(&scrubbed);
        assert_eq!(
            serde_json::from_str::<Value>(&restored).unwrap(),
            serde_json::from_str::<Value>(&original).unwrap()
        );
    }

    #[test]
    fn redacts_tool_call_arguments_and_restores_them() {
        let arguments = json!({
            "path": "notes.txt",
            "content": "api_key=sk-live-abcdef1234567890",
        })
        .to_string();
        let message = ChatMessage::assistant_with_tools(
            "writing",
            vec![ToolCall {
                id: "call_1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "write_file".into(),
                    arguments: arguments.clone(),
                },
            }],
        );
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact_message(&message);
        let scrubbed_args = &scrubbed.tool_calls.as_ref().unwrap()[0].function.arguments;
        assert!(!scrubbed_args.contains("sk-live-abcdef1234567890"));
        assert!(scrubbed_args.contains("{{eix_secret_"));

        let restored = redactor.restore_json(scrubbed_args);
        assert_eq!(
            serde_json::from_str::<Value>(&restored).unwrap(),
            serde_json::from_str::<Value>(&arguments).unwrap()
        );
    }

    #[test]
    fn ordinary_prose_status_words_are_not_redacted() {
        let mut redactor = SecretRedactor::new("test-salt");
        for text in [
            "the token is invalid",
            "the api key is missing",
            "the password is required",
        ] {
            assert_eq!(redactor.redact(text), text);
        }
    }

    #[test]
    fn two_pass_scrubbing_catches_earlier_bare_mentions() {
        let messages = vec![
            ChatMessage::user("the password is hunter2"),
            ChatMessage::user("PASSWORD=hunter2"),
        ];
        let mut redactor = SecretRedactor::new("test-salt");
        let scrubbed = redactor.redact_messages(&messages);
        assert!(!scrubbed[0].content.contains("hunter2"), "{scrubbed:?}");
        assert!(
            scrubbed[0].content.contains("{{eix_secret_"),
            "{scrubbed:?}"
        );
        assert_eq!(
            redactor.restore(&scrubbed[0].content),
            "the password is hunter2"
        );
    }

    #[test]
    fn known_secrets_are_scrubbed_even_without_their_key() {
        let mut redactor = SecretRedactor::new("test-salt");
        redactor.redact("PASSWORD=hunter2");
        let scrubbed = redactor.redact("copying hunter2 into a file");
        assert!(!scrubbed.contains("hunter2"), "{scrubbed}");
        assert!(scrubbed.contains("{{eix_secret_"), "{scrubbed}");

        // Do not turn a known short secret into a substring replacement that
        // corrupts the next key and lets its value through.
        let mut redactor = SecretRedactor::new("test-salt");
        redactor.redact("password=pass");
        let scrubbed = redactor.redact("password=hunter3");
        assert!(!scrubbed.contains("hunter3"), "{scrubbed}");
        assert!(scrubbed.contains("password="), "{scrubbed}");
        assert_eq!(redactor.restore(&scrubbed), "password=hunter3");
    }

    #[test]
    fn same_secret_gets_a_stable_placeholder() {
        let mut redactor = SecretRedactor::new("test-salt");
        let first = redactor.redact("password=hunter2");
        let second = redactor.redact("token: hunter2");
        let first_id = first
            .split("{{eix_secret_")
            .nth(1)
            .and_then(|rest| rest.split("}}").next())
            .unwrap();
        let second_id = second
            .split("{{eix_secret_")
            .nth(1)
            .and_then(|rest| rest.split("}}").next())
            .unwrap();
        assert_eq!(first_id, second_id);
    }

    #[test]
    fn code_type_annotations_are_not_secrets() {
        let mut redactor = SecretRedactor::new("test-salt");
        let code = r#"pub(crate) async fn serve(daemon: Arc<Daemon>, token: Option<String>) {
    let token: String = token.unwrap_or_default();
    if token.is_empty() {}
}
struct Credentials {
    token: SecretString
}"#;
        assert_eq!(redactor.redact(code), code);

        let comment = "// token: SecretString";
        assert_eq!(redactor.redact(comment), comment);

        // A type name learned from one tool result must not turn into a
        // process-wide replacement that corrupts every later Rust snippet.
        let later = "Option, String, and SecretString are ordinary Rust types.";
        assert_eq!(redactor.redact(later), later);
    }

    #[test]
    fn code_tokens_are_not_globally_replaced() {
        let mut redactor = SecretRedactor::new("test-salt");
        let assignment = redactor.redact("password=\"Option\"");
        assert!(!assignment.contains("\"Option\""), "{assignment}");

        let later = "Option<String>";
        assert_eq!(redactor.redact(later), later);
    }

    #[test]
    fn code_status_prose_is_not_redacted() {
        let mut redactor = SecretRedactor::new("test-salt");
        for text in [
            "the token is checked",
            "the secret is stored",
            "the password is validated",
        ] {
            assert_eq!(redactor.redact(text), text);
        }
    }

    #[test]
    fn config_assignments_are_still_redacted() {
        let mut redactor = SecretRedactor::new("test-salt");
        for (text, secret) in [
            ("PASSWORD=hunter2", "hunter2"),
            ("token: hunter2", "hunter2"),
            ("token: \"hunter2\"", "hunter2"),
            ("let password = \"hunter2\";", "hunter2"),
            ("  password: Hunter", "Hunter"),
            ("token: hunter2,", "hunter2"),
            ("token: hunter2 # comment", "hunter2"),
        ] {
            let scrubbed = redactor.redact(text);
            assert!(!scrubbed.contains(secret), "{text} -> {scrubbed}");
            assert!(scrubbed.contains("{{eix_secret_"), "{text} -> {scrubbed}");
        }
    }
}
