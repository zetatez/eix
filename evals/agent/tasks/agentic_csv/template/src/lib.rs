pub fn split_line(line: &str) -> Vec<String> {
    line.split(',').map(|s| s.trim_matches('"').to_string()).collect()
}

pub fn parse(input: &str) -> Vec<Vec<String>> {
    input.lines().map(split_line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_plain_fields() {
        assert_eq!(split_line("a,b,c"), vec!["a", "b", "c"]);
    }
}
