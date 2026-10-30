use plan_refactor::{subtotal_a, subtotal_b};

#[test]
fn sums_prices() {
    assert_eq!(subtotal_a(&[1, 2, 3]), 6);
    assert_eq!(subtotal_b(&[4, 5]), 9);
    assert_eq!(subtotal_a(&[]), 0);
}

#[test]
fn saturates_on_overflow() {
    assert_eq!(subtotal_a(&[i64::MAX, 1]), i64::MAX);
    assert_eq!(subtotal_b(&[i64::MIN, -1]), i64::MIN);
}
