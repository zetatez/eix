pub fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    haystack.find(needle).map(|index| vec![index]).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_one_match() {
        assert_eq!(find_all("abc", "bc"), vec![1]);
    }
}
