pub fn isqrt(n: u64) -> u64 {
    (n as f64).sqrt() as u64
}

pub fn is_perfect_square(n: u64) -> bool {
    let root = (n as f64).sqrt() as u64;
    root * root == n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_squares() {
        assert_eq!(isqrt(16), 4);
    }
}
