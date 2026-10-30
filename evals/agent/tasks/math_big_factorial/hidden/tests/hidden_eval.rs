use big_factorial::factorial;

#[test]
fn computes_exact_factorials() {
    assert_eq!(factorial(0), Some("1".to_string()));
    assert_eq!(factorial(1), Some("1".to_string()));
    assert_eq!(factorial(5), Some("120".to_string()));
    assert_eq!(factorial(20), Some("2432902008176640000".to_string()));
    assert_eq!(factorial(25), Some("15511210043330985984000000".to_string()));
}

#[test]
fn rejects_huge_inputs() {
    assert_eq!(factorial(10_001), None);
}
