pub fn binom_mod(n: u64, k: u64, p: u64) -> Option<u64> {
    if p < 2 {
        return None;
    }
    Some(if k > n { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_binomial() {
        assert_eq!(binom_mod(10, 5, 1_000_000_007), Some(252));
    }
}
