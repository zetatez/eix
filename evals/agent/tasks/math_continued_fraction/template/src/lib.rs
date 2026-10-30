pub fn from_fraction(coefficients: &[i64]) -> Option<f64> {
    coefficients.first().map(|&value| value as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_coefficient() {
        assert_eq!(from_fraction(&[2]), Some(2.0));
    }
}
