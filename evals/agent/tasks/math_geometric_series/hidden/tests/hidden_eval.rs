use geometric_series::sum_geometric;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn sums_finite_series() {
    close(sum_geometric(1.0, 2.0, 4), 15.0);
    close(sum_geometric(1.0, 0.5, 3), 1.75);
    close(sum_geometric(1.0, -1.0, 4), 0.0);
    close(sum_geometric(1.0, -1.0, 3), 1.0);
}

#[test]
fn handles_degenerate_cases() {
    close(sum_geometric(5.0, 1.0, 3), 15.0);
    close(sum_geometric(5.0, 2.0, 0), 0.0);
    assert_eq!(sum_geometric(f64::NAN, 2.0, 3), None);
    assert_eq!(sum_geometric(1.0, f64::INFINITY, 3), None);
}
