use triangle::{area, classify};

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn classifies_triangles() {
    assert_eq!(classify(3.0, 4.0, 5.0), Some("scalene"));
    assert_eq!(classify(2.0, 2.0, 3.0), Some("isosceles"));
    assert_eq!(classify(2.0, 2.0, 2.0), Some("equilateral"));
}

#[test]
fn rejects_invalid_triangles() {
    assert_eq!(classify(1.0, 2.0, 3.0), None);
    assert_eq!(classify(0.0, 1.0, 1.0), None);
    assert_eq!(classify(f64::NAN, 1.0, 1.0), None);
    assert_eq!(area(1.0, 2.0, 3.0), None);
    assert_eq!(area(-1.0, 2.0, 2.0), None);
}

#[test]
fn computes_area() {
    close(area(3.0, 4.0, 5.0).unwrap(), 6.0);
    close(area(2.0, 2.0, 2.0).unwrap(), 3.0_f64.sqrt());
}
