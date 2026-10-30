use catalan::catalan;

#[test]
fn matches_known_catalan_numbers() {
    assert_eq!(catalan(0), Some(1));
    assert_eq!(catalan(1), Some(1));
    assert_eq!(catalan(5), Some(42));
    assert_eq!(catalan(10), Some(16796));
    assert_eq!(catalan(19), Some(1_767_263_190));
}

#[test]
fn largest_fitting_value() {
    assert_eq!(catalan(36), Some(11_959_798_385_860_453_492));
}

#[test]
fn reports_overflow() {
    assert_eq!(catalan(37), None);
}
