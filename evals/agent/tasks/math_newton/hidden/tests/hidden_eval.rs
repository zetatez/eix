use newton::sqrt_newton;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn computes_square_roots() {
    close(sqrt_newton(0.0), 0.0);
    close(sqrt_newton(1.0), 1.0);
    close(sqrt_newton(9.0), 3.0);
    close(sqrt_newton(2.0), std::f64::consts::SQRT_2);
    close(sqrt_newton(1e-12), 1e-6);
    close(sqrt_newton(1e12), 1e6);
}

#[test]
fn rejects_invalid_input() {
    assert_eq!(sqrt_newton(-1.0), None);
    assert_eq!(sqrt_newton(f64::NAN), None);
    assert_eq!(sqrt_newton(f64::INFINITY), None);
}
