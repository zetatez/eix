//! Best-effort secret redaction for daemon logs and error messages.
//!
//! There is no single place where every error string flows through a logger,
//! so callers scrub the final message before `eprintln!`. The patterns cover
//! the realistic leaks: `Bearer` headers, `sk-…` API keys, and the values of
//! `token`, `api_key`, `password`, `secret` and `authorization` in JSON or
//! `KEY=value` form. Redaction is deliberately conservative: a false positive
//! only hides a value, while a miss writes a secret to disk.

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

#[cfg(test)]
mod tests {
    use super::redact;

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
}
