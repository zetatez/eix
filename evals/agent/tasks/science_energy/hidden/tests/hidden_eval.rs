use energy::{kinetic, potential};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn computes_values() {
    assert_eq!(kinetic(2.0, 3.0), Ok(9.0));
    close(potential(2.0, 5.0, 9.81).unwrap(), 98.1);
    close(potential(2.0, -1.0, 9.81).unwrap(), -19.62);
}

#[test]
fn rejects_bad_inputs() {
    assert_eq!(kinetic(-1.0, 3.0), Err("invalid"));
    assert_eq!(kinetic(1.0, -3.0), Err("invalid"));
    assert_eq!(kinetic(f64::NAN, 3.0), Err("invalid"));
    assert_eq!(potential(1.0, 1.0, 0.0), Err("invalid"));
    assert_eq!(potential(-1.0, 1.0, 9.81), Err("invalid"));
}
