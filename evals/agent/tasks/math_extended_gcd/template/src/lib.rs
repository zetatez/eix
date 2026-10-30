pub fn extended_gcd(a: i64, b: i64) -> (i64, i64, i64) {
    (a.abs(), 1, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_gcd() {
        let (g, x, y) = extended_gcd(4, 2);
        assert_eq!(g, 2);
        assert_eq!(4 * x + 2 * y, g);
    }
}
