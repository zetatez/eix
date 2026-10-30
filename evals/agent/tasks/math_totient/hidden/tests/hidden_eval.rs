use totient::phi;

#[test]
fn counts_coprimes() {
    assert_eq!(phi(1), Some(1));
    assert_eq!(phi(2), Some(1));
    assert_eq!(phi(9), Some(6));
    assert_eq!(phi(10), Some(4));
    assert_eq!(phi(12), Some(4));
    assert_eq!(phi(97), Some(96));
}

#[test]
fn handles_products_and_zero() {
    assert_eq!(phi(30), Some(8));
    assert_eq!(phi(100), Some(40));
    assert_eq!(phi(0), None);
}
