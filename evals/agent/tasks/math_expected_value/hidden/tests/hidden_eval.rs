use expected_value::mean_variance;

fn close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn computes_mean_and_variance() {
    let (mean, variance) = mean_variance(&[0.25, 0.75], &[0.0, 2.0]).unwrap();
    close(mean, 1.5);
    close(variance, 0.75);

    let (mean, variance) = mean_variance(&[0.5, 0.5], &[1.0, 3.0]).unwrap();
    close(mean, 2.0);
    close(variance, 1.0);

    let (mean, variance) = mean_variance(&[1.0], &[7.0]).unwrap();
    close(mean, 7.0);
    close(variance, 0.0);
}

#[test]
fn rejects_invalid_distributions() {
    assert_eq!(mean_variance(&[0.5, 0.6], &[1.0, 2.0]), None);
    assert_eq!(mean_variance(&[1.0], &[]), None);
    assert_eq!(mean_variance(&[], &[]), None);
    assert_eq!(mean_variance(&[-0.5, 1.5], &[1.0, 2.0]), None);
    assert_eq!(mean_variance(&[1.0], &[f64::NAN]), None);
}
