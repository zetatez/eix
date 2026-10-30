mod lexer;
mod parser;

pub use lexer::Token;
pub use parser::eval_expr;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(eval_expr("1 + 2"), Some(3));
    }
}
