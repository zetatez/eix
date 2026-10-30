pub fn divisors(n: u64) -> Vec<u64> {
    (1..=n / 2).filter(|value| n % value == 0).collect()
}

pub fn sum_proper_divisors(n: u64) -> u64 {
    divisors(n).iter().sum()
}

pub fn is_perfect(n: u64) -> bool {
    sum_proper_divisors(n) == n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_small_divisors() {
        assert!(divisors(12).contains(&6));
    }
}
