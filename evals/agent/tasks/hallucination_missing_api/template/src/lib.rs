/// Parse a trimmed decimal integer, rejecting values that do not fit `i32`.
pub fn parse_v1(input: &str) -> Option<i32> {
    input.trim().parse::<i32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_small_values() {
        assert_eq!(parse_v1("42"), Some(42));
        assert_eq!(parse_v1("  -7 "), Some(-7));
        assert_eq!(parse_v1("nope"), None);
    }
}
