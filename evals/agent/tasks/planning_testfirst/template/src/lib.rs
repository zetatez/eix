pub fn slugify(input: &str) -> String {
    input.replace(' ', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_collapses() {
        assert_eq!(slugify("Hello,  World!"), "hello-world");
    }
}
