use bayes::posterior;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn applies_bayes_theorem() {
    close(posterior(0.01, 0.99, 0.05), 0.99 * 0.01 / (0.99 * 0.01 + 0.05 * 0.99));
    close(posterior(0.5, 1.0, 1.0), 0.5);
    close(posterior(0.0, 0.9, 0.1), 0.0);
    close(posterior(1.0, 0.9, 0.1), 1.0);
}

#[test]
fn rejects_invalid_inputs() {
    assert_eq!(posterior(-0.1, 0.9, 0.1), None);
    assert_eq!(posterior(0.5, 1.1, 0.1), None);
    assert_eq!(posterior(0.5, 0.9, f64::NAN), None);
    assert_eq!(posterior(0.0, 0.0, 0.0), None);
}
