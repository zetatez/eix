pub fn sum_phi(n: u64) -> u64 {
    n * (n + 1) / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_sum() {
        assert_eq!(sum_phi(0), 0);
    }
}
