pub fn escape(input: &str) -> String {
    input.replace('"', "\\\"")
}

pub fn unescape(input: &str) -> Option<String> {
    Some(input.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_quotes() {
        assert_eq!(escape("a\"b"), "a\\\"b");
    }
}
