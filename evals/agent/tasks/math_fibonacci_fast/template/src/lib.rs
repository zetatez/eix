pub fn fib_mod(n: u64, modulus: u64) -> Option<u64> {
    if modulus == 0 {
        return None;
    }
    Some(n % modulus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_zero() {
        assert_eq!(fib_mod(0, 1000), Some(0));
    }
}
