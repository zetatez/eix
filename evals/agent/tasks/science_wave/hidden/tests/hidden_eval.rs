use wave::{frequency, wavelength};

#[test]
fn computes_values() {
    assert_eq!(wavelength(10.0, 2.0), Ok(5.0));
    assert_eq!(frequency(10.0, 5.0), Ok(2.0));
}

#[test]
fn rejects_bad_inputs() {
    assert_eq!(wavelength(0.0, 2.0), Err("invalid"));
    assert_eq!(wavelength(10.0, -2.0), Err("invalid"));
    assert_eq!(frequency(10.0, 0.0), Err("invalid"));
    assert_eq!(frequency(f64::INFINITY, 1.0), Err("invalid"));
}
