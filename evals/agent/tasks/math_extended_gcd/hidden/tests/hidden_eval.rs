use extended_gcd::extended_gcd;

fn identity(a: i64, b: i64, g: i64, x: i64, y: i64) {
    assert_eq!(g, g.abs());
    assert_eq!(a as i128 * x as i128 + b as i128 * y as i128, g as i128);
}

#[test]
fn computes_coefficients() {
    let (g, x, y) = extended_gcd(240, 46);
    assert_eq!(g, 2);
    identity(240, 46, g, x, y);

    let (g, x, y) = extended_gcd(-240, 46);
    assert_eq!(g, 2);
    identity(-240, 46, g, x, y);

    let (g, x, y) = extended_gcd(5, 0);
    assert_eq!((g, x, y), (5, 1, 0));
}

#[test]
fn handles_zero_cases() {
    assert_eq!(extended_gcd(0, 0), (0, 0, 0));
    assert_eq!(extended_gcd(0, 5), (5, 0, 1));
}
