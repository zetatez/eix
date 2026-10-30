use linear::solve;

fn close(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-9, "{a} vs {b}");
}

#[test]
fn solves_2x2() {
    let (x, y) = solve(2.0, 3.0, 1.0, -1.0, 7.0, 1.0).unwrap();
    close(x, 2.0);
    close(y, 1.0);
}

#[test]
fn singular_and_non_finite_are_none() {
    assert_eq!(solve(1.0, 2.0, 2.0, 4.0, 3.0, 6.0), None);
    assert_eq!(solve(f64::NAN, 0.0, 0.0, 1.0, 1.0, 1.0), None);
}
