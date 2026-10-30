mod tokenize;
mod transform;

pub use tokenize::tokenize;
pub use transform::transform;

/// Normalize `input` and join the surviving tokens with single spaces.
pub fn run(input: &str) -> String {
    todo!("compose tokenize and transform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases_and_joins() {
        assert_eq!(run("Hello There"), "hello there");
    }
}
