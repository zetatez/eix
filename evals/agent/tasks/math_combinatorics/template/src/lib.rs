/// Exact factorial of `n`, panicking with `overflow` if it exceeds `u128`.
pub fn factorial(n: u64) -> u128 {
    let mut acc: u128 = 1;
    for i in 1..=n {
        acc *= i as u128;
    }
    acc
}

/// Exact binomial coefficient `n choose k`.
pub fn binomial(n: u64, k: u64) -> u128 {
    factorial(n) / (factorial(k) * factorial(n - k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values() {
        assert_eq!(factorial(5), 120);
        assert_eq!(binomial(5, 2), 10);
    }
}
