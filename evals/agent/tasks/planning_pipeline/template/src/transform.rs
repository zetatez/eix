/// Drop the stopwords `the`, `a` and `an`.
pub fn transform(tokens: Vec<String>) -> Vec<String> {
    tokens
        .into_iter()
        .filter(|token| !matches!(token.as_str(), "the" | "a" | "an"))
        .collect()
}
