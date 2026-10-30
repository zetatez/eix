/// One parsed `key=value` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The line had no `=`.
    MissingEquals(String),
    /// The key was empty after trimming.
    EmptyKey(String),
    /// The key appeared more than once.
    DuplicateKey(String),
}

/// Parse `key=value` lines. Blank lines and `#` comments are skipped.
pub fn parse_config(input: &str) -> Result<Vec<Entry>, ConfigError> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .ok_or_else(|| ConfigError::MissingEquals(trimmed.to_string()))?;
        entries.push(Entry {
            key: key.trim().to_string(),
            value: value.trim().to_string(),
        });
    }
    Ok(entries)
}
