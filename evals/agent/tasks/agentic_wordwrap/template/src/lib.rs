pub fn wrap(text: &str, width: usize) -> Vec<String> {
    vec![text.to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_a_phrase() {
        assert_eq!(wrap("the quick brown fox", 10), vec!["the quick", "brown fox"]);
    }
}
