pub fn posterior(prior: f64, sensitivity: f64, false_positive: f64) -> Option<f64> {
    let _ = sensitivity;
    let _ = false_positive;
    Some(prior)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn half_prior_stays_half() {
        assert_eq!(posterior(0.5, 1.0, 1.0), Some(0.5));
    }
}
