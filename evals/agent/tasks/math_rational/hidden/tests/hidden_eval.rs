use rational::Rational;

fn r(num: i64, den: i64) -> Rational {
    Rational::new(num, den)
}

#[test]
fn reduces_on_construction() {
    assert_eq!(r(2, 4), r(1, 2));
    assert_eq!(r(-2, -4), r(1, 2));
    assert_eq!(r(2, -4), r(-1, 2));
    assert_eq!(r(0, 5), r(0, 1));
}

#[test]
#[should_panic(expected = "zero denominator")]
fn zero_denominator_panics() {
    let _ = r(1, 0);
}

#[test]
fn add_and_mul_reduce() {
    assert_eq!(r(1, 3).add(r(1, 6)), r(1, 2));
    assert_eq!(r(2, 3).mul(r(3, 4)), r(1, 2));
    assert_eq!(r(-1, 2).add(r(1, 2)), r(0, 1));
    assert_eq!(r(-2, 3).mul(r(3, -2)), r(1, 1));
}

#[test]
fn large_cross_products_fit_after_reduction() {
    // Cross-products exceed i64, but the reduced result fits.
    let max = i64::MAX;
    let sum = r(max, 2).add(r(max, 2));
    assert_eq!(sum, r(max, 1));
    assert_eq!(sum.to_f64(), max as f64);
}

#[test]
fn to_f64_is_the_quotient() {
    assert_eq!(r(3, 2).to_f64(), 1.5);
    assert_eq!(r(-3, 2).to_f64(), -1.5);
}
