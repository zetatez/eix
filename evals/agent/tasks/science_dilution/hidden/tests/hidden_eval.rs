use dilution::{final_concentration, required_volume};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn dilution_math() {
    close(final_concentration(2.0, 0.1, 0.2).unwrap(), 1.0);
    close(required_volume(2.0, 0.1, 0.5).unwrap(), 0.4);
}

#[test]
fn invalid_inputs() {
    assert_eq!(final_concentration(0.0, 1.0, 1.0), Err("invalid"));
    assert_eq!(final_concentration(1.0, 1.0, 0.0), Err("invalid"));
    assert_eq!(final_concentration(1.0, f64::NAN, 1.0), Err("invalid"));
    assert_eq!(required_volume(1.0, -1.0, 1.0), Err("invalid"));
    assert_eq!(required_volume(1.0, 1.0, f64::INFINITY), Err("invalid"));
}
