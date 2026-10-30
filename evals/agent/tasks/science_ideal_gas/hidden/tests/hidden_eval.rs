use gas::{pressure, temperature, volume, R};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() / b.abs().max(1.0) < 1e-9, "{a} vs {b}");
}

#[test]
fn ideal_gas_values() {
    close(pressure(1.0, 273.15, 0.0224).unwrap(), R * 273.15 / 0.0224);
    close(volume(1.0, 273.15, 101_325.0).unwrap(), R * 273.15 / 101_325.0);
    close(temperature(101_325.0, 0.0224, 1.0).unwrap(), 101_325.0 * 0.0224 / R);
}

#[test]
fn rejects_bad_inputs() {
    assert_eq!(pressure(0.0, 300.0, 1.0), Err("invalid"));
    assert_eq!(pressure(1.0, -1.0, 1.0), Err("invalid"));
    assert_eq!(pressure(1.0, 300.0, 0.0), Err("invalid"));
    assert_eq!(volume(1.0, 300.0, f64::NAN), Err("invalid"));
    assert_eq!(temperature(1.0, 1.0, f64::INFINITY), Err("invalid"));
}
