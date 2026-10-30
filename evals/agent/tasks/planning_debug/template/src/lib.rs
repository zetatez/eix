pub mod stats;
pub mod text;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke() {
        assert_eq!(stats::sum(&[1, 2, 3]), 6);
        assert_eq!(text::word_count("a b c"), 3);
    }
}
