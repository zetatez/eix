use continued_fraction::from_fraction;

fn close(actual: Option<f64>, expected: f64) {
    let actual = actual.expect("expected a value");
    assert!((actual - expected).abs() < 1e-9, "{actual} vs {expected}");
}

#[test]
fn evaluates_continued_fractions() {
    close(from_fraction(&[1, 2, 3]), 10.0 / 7.0);
    close(from_fraction(&[2]), 2.0);
    close(from_fraction(&[0, 1]), 1.0);
    close(from_fraction(&[1, 1, 1, 1]), 5.0 / 3.0);
    close(from_fraction(&[3, 7, 15, 1, 292]), std::f64::consts::PI);
}

#[test]
fn rejects_undefined_input() {
    assert_eq!(from_fraction(&[]), None);
    assert_eq!(from_fraction(&[1, 0]), None);
    assert_eq!(from_fraction(&[2, 3, 0]), None);
}
