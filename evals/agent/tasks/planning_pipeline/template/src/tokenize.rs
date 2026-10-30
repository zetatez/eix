/// Split on whitespace, trim surrounding ASCII punctuation, lowercase, and
/// drop tokens that become empty.
pub fn tokenize(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|c: char| c.is_ascii_punctuation())
                .to_ascii_lowercase()
        })
        .filter(|word| !word.is_empty())
        .collect()
}
