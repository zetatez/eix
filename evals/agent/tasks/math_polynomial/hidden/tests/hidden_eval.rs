use polynomial::{derivative, eval};

#[test]
fn evaluates_polynomials() {
    assert_eq!(eval(&[1, 2, 3], 2), Some(17));
    assert_eq!(eval(&[], 5), Some(0));
    assert_eq!(eval(&[7], 5), Some(7));
    assert_eq!(eval(&[-1, 1], 3), Some(2));
}

#[test]
fn reports_overflow() {
    assert_eq!(eval(&[i64::MAX, 1], 2), None);
}

#[test]
fn differentiates() {
    assert_eq!(derivative(&[]), Vec::<i64>::new());
    assert_eq!(derivative(&[5]), Vec::<i64>::new());
    assert_eq!(derivative(&[1, 2, 3]), vec![2, 6]);
}
