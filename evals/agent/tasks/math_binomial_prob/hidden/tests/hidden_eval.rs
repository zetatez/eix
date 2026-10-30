use binomial_prob::binomial_probability;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn computes_binomial_masses() {
    close(binomial_probability(10, 5, 0.5), 0.24609375);
    close(binomial_probability(10, 0, 0.3), 0.7f64.powi(10));
    close(binomial_probability(3, 2, 0.5), 0.375);
    close(binomial_probability(10, 10, 1.0), 1.0);
    close(binomial_probability(0, 0, 0.0), 1.0);
}

#[test]
fn rejects_invalid_input() {
    assert_eq!(binomial_probability(3, 4, 0.5), None);
    assert_eq!(binomial_probability(3, 1, -0.1), None);
    assert_eq!(binomial_probability(3, 1, 1.1), None);
    assert_eq!(binomial_probability(3, 1, f64::NAN), None);
}
