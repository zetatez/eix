pub fn word_count(input: &str) -> usize {
    input.split(',').count()
}

pub fn first_word(input: &str) -> Option<&str> {
    input.split_whitespace().next()
}
