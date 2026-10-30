pub fn matches(pattern: &str, text: &str) -> bool {
    pattern == text || pattern == "*"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_exact_text() {
        assert!(matches("abc", "abc"));
    }
}
