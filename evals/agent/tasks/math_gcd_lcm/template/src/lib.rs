/// Greatest common divisor, always non-negative. `gcd(0, 0)` is 0.
pub fn gcd(a: i64, b: i64) -> u64 {
    (a % b).unsigned_abs()
}

/// Least common multiple as `u64`, or `None` when it does not fit.
pub fn lcm(a: i64, b: i64) -> Option<u64> {
    Some((a * b / gcd(a, b) as i64).unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values() {
        assert_eq!(gcd(12, 8), 4);
        assert_eq!(lcm(3, 4), Some(12));
    }
}
