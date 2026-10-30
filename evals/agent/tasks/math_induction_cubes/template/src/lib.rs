pub fn sum_cubes(n: u64) -> Option<u64> {
    Some(n * n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_is_zero() {
        assert_eq!(sum_cubes(0), Some(0));
    }
}
