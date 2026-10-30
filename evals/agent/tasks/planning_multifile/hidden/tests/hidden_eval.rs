use plan_multifile::{eval_expr, Token};

#[test]
fn evaluates_three_operators() {
    assert_eq!(eval_expr("1 + 2"), Some(3));
    assert_eq!(eval_expr("9 - 4"), Some(5));
    assert_eq!(eval_expr("6 * 7"), Some(42));
}

#[test]
fn rejects_malformed_and_overflow() {
    assert_eq!(eval_expr("1+2"), None);
    assert_eq!(eval_expr("1 + "), None);
    assert_eq!(eval_expr("a + 2"), None);
    assert_eq!(eval_expr("9223372036854775807 + 1"), None);
    assert_eq!(Token::Number(3), Token::Number(3));
}
