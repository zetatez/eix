pub fn parse_v1(input: &str) -> Option<i64> {
    input.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimal() {
        assert_eq!(parse_v1("42"), Some(42));
    }
}
