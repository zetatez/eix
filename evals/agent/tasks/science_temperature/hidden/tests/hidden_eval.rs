use temperature::*;

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn standard_points() {
    close(celsius_to_fahrenheit(100.0).unwrap(), 212.0);
    close(fahrenheit_to_celsius(32.0).unwrap(), 0.0);
    close(celsius_to_kelvin(-273.15).unwrap(), 0.0);
    close(kelvin_to_celsius(273.15).unwrap(), 0.0);
}

#[test]
fn rejects_non_finite() {
    assert_eq!(celsius_to_fahrenheit(f64::NAN), Err("finite"));
    assert_eq!(celsius_to_kelvin(f64::INFINITY), Err("finite"));
    assert_eq!(fahrenheit_to_celsius(f64::NAN), Err("finite"));
}

#[test]
fn rejects_below_absolute_zero() {
    assert_eq!(celsius_to_kelvin(-273.16), Err("below absolute zero"));
    assert_eq!(celsius_to_fahrenheit(-300.0), Err("below absolute zero"));
    assert_eq!(kelvin_to_celsius(-0.1), Err("below absolute zero"));
}
