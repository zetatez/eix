use simpson::integrate;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn integrates_polynomials_exactly() {
    close(integrate(|x| x * x, 0.0, 1.0, 2), 1.0 / 3.0);
    close(integrate(|x| x * x, 0.0, 1.0, 4), 1.0 / 3.0);
    close(integrate(|x| 3.0 * x * x + 2.0 * x + 1.0, 0.0, 2.0, 2), 14.0);
}

#[test]
fn integrates_transcendental_functions() {
    close(integrate(|x| x.sin(), 0.0, std::f64::consts::PI, 1000), 2.0);
}

#[test]
fn rejects_invalid_arguments() {
    assert_eq!(integrate(|x| x, 0.0, 1.0, 0), None);
    assert_eq!(integrate(|x| x, 0.0, 1.0, 3), None);
    assert_eq!(integrate(|x| x, f64::NAN, 1.0, 2), None);
    assert_eq!(integrate(|x| x, 0.0, f64::INFINITY, 2), None);
}
