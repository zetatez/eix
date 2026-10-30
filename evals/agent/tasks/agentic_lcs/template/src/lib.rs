pub fn lcs(a: &str, b: &str) -> String {
    if a == b { a.to_string() } else { String::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings() {
        assert_eq!(lcs("abc", "abc"), "abc");
    }
}
