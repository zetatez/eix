use totals::{total_with_discount, total_with_tax};

#[test]
fn empty_cart_is_zero() {
    assert_eq!(total_with_tax(&[], 5_000), 0);
    assert_eq!(total_with_discount(&[], 5_000), 0);
}

#[test]
fn tax_saturates_instead_of_overflowing() {
    assert_eq!(total_with_tax(&[u32::MAX], 10_000), u32::MAX);
    assert_eq!(total_with_tax(&[u32::MAX, u32::MAX], 0), u32::MAX);
}

#[test]
fn discount_clamps_at_zero() {
    assert_eq!(total_with_discount(&[100], 20_000), 0);
    assert_eq!(total_with_discount(&[100], 10_000), 0);
    assert_eq!(total_with_discount(&[100], 0), 100);
}

#[test]
fn normal_cases_are_unchanged() {
    assert_eq!(total_with_tax(&[100, 200], 1_000), 330);
    assert_eq!(total_with_discount(&[100, 200], 1_000), 270);
}
