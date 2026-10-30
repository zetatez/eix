pub fn checked_div(a: i64, b: i64) -> Result<i64, &'static str> {
    if b == 0 {
        Err("divide by zero")
    } else {
        Ok(a / b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divides() {
        assert_eq!(checked_div(8, 2), Ok(4));
        assert_eq!(checked_div(1, 0), Err("divide by zero"));
    }
}
