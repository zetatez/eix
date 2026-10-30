pub fn truncate(input: &str, max_bytes: usize, suffix: &str) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let keep = max_bytes.saturating_sub(suffix.len());
    format!("{}{}", &input[..keep], suffix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_suffix() {
        assert_eq!(truncate("hello world", 8, "..."), "hello...");
    }
}
