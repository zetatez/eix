#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    InvalidDigit,
    TooLarge,
}

pub fn validate(n: i64) -> Result<(), ParseError> {
    if n == 0 {
        Err(ParseError::Empty)
    } else if n < 0 {
        Err(ParseError::InvalidDigit)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty() {
        assert_eq!(validate(0), Err(ParseError::Empty));
    }
}
