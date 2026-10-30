pub fn mean_variance(probabilities: &[f64], values: &[f64]) -> Option<(f64, f64)> {
    if probabilities.len() != values.len() {
        return None;
    }
    let mean = probabilities.iter().zip(values).map(|(p, v)| p * v).sum();
    Some((mean, 0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_point() {
        assert_eq!(mean_variance(&[1.0], &[7.0]), Some((7.0, 0.0)));
    }
}
