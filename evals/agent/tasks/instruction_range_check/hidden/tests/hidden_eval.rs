use instr_range::{validate, ParseError};

#[test]
fn rejects_values_above_100() {
    assert_eq!(validate(100), Ok(()));
    assert_eq!(validate(101), Err(ParseError::TooLarge));
    assert_eq!(validate(0), Err(ParseError::Empty));
    assert_eq!(validate(-1), Err(ParseError::InvalidDigit));
}
