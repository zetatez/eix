pub fn distance(a: &str, b: &str) -> usize {
    if a == b { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_strings_are_zero() {
        assert_eq!(distance("abc", "abc"), 0);
    }
}
