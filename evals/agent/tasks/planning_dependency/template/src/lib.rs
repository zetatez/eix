pub fn parse(input: &str) -> Vec<(String, i64)> {
    todo!()
}

pub fn summarize(input: &str) -> String {
    format!("{}", input.lines().count())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_two_rows() {
        assert_eq!(summarize("a:1\nb:2"), "a=1, b=2, total=3");
    }
}
