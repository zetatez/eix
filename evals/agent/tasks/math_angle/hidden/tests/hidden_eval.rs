use angle::{normalize_deg, to_degrees, to_radians};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn normalizes_into_range() {
    assert_eq!(normalize_deg(0.0), Some(0.0));
    assert_eq!(normalize_deg(360.0), Some(0.0));
    assert_eq!(normalize_deg(-90.0), Some(270.0));
    assert_eq!(normalize_deg(-0.5), Some(359.5));
    assert_eq!(normalize_deg(720.5), Some(0.5));
    assert_eq!(normalize_deg(f64::NAN), None);
    assert_eq!(normalize_deg(f64::INFINITY), None);
}

#[test]
fn converts_units() {
    close(to_radians(180.0).unwrap(), std::f64::consts::PI);
    close(to_degrees(std::f64::consts::PI).unwrap(), 180.0);
    assert_eq!(to_radians(f64::NAN), None);
    assert_eq!(to_degrees(f64::INFINITY), None);
}
