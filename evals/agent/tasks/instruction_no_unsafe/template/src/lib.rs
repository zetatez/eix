pub fn checked_abs(x: i64) -> Option<i64> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs() {
        assert_eq!(checked_abs(-3), Some(3));
    }
}
