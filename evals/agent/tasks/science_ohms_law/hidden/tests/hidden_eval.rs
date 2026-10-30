use ohms::{current, power, voltage};

#[test]
fn computes_values() {
    assert_eq!(voltage(2.0, 3.0), Ok(6.0));
    assert_eq!(current(6.0, 3.0), Ok(2.0));
    assert_eq!(power(6.0, 2.0), Ok(12.0));
}

#[test]
fn rejects_bad_inputs() {
    assert_eq!(voltage(2.0, 0.0), Err("invalid"));
    assert_eq!(voltage(-1.0, 3.0), Err("invalid"));
    assert_eq!(current(6.0, -3.0), Err("invalid"));
    assert_eq!(current(f64::NAN, 3.0), Err("invalid"));
    assert_eq!(power(6.0, f64::INFINITY), Err("invalid"));
}
