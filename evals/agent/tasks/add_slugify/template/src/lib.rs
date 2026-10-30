/// Turn arbitrary text into a URL-friendly slug.
///
/// Lowercase ASCII letters and digits are kept. Any run of other characters
/// becomes one `-`, with no leading or trailing dash.
pub fn slugify(input: &str) -> String {
    todo!("implement slugify")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punctuation_becomes_one_dash() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
    }
}
