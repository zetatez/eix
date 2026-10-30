use combinatorics::{binomial, factorial};

#[test]
fn factorial_is_exact() {
    assert_eq!(factorial(0), 1);
    assert_eq!(factorial(1), 1);
    assert_eq!(factorial(10), 3_628_800);
    assert_eq!(
        factorial(34),
        295_232_799_039_604_140_847_618_609_643_520_000_000
    );
}

#[test]
#[should_panic(expected = "overflow")]
fn factorial_reports_overflow() {
    let _ = factorial(35);
}

#[test]
fn binomial_edges() {
    assert_eq!(binomial(0, 0), 1);
    assert_eq!(binomial(5, 6), 0);
    assert_eq!(binomial(5, 0), 1);
    assert_eq!(binomial(5, 5), 1);
}

#[test]
fn binomial_is_exact_for_large_values() {
    assert_eq!(binomial(52, 5), 2_598_960);
    assert_eq!(binomial(100, 50), 100_891_344_545_564_193_334_812_497_256);
}
