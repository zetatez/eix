use molarity::{molarity, moles};

#[test]
fn computes_values() {
    assert_eq!(moles(36.0, 18.0), Ok(2.0));
    assert_eq!(molarity(2.0, 0.5), Ok(4.0));
}

#[test]
fn rejects_bad_inputs() {
    assert_eq!(moles(0.0, 18.0), Err("invalid"));
    assert_eq!(moles(36.0, -18.0), Err("invalid"));
    assert_eq!(molarity(2.0, 0.0), Err("invalid"));
    assert_eq!(molarity(f64::NAN, 1.0), Err("invalid"));
}
