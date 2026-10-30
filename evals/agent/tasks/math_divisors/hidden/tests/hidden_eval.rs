use divisors::{divisors, is_perfect, sum_proper_divisors};

#[test]
fn lists_divisors_ascending() {
    assert_eq!(divisors(1), vec![1]);
    assert_eq!(divisors(12), vec![1, 2, 3, 4, 6, 12]);
    assert_eq!(divisors(13), vec![1, 13]);
    assert_eq!(divisors(0), Vec::new());
}

#[test]
fn sums_proper_divisors() {
    assert_eq!(sum_proper_divisors(6), 6);
    assert_eq!(sum_proper_divisors(1), 0);
    assert_eq!(sum_proper_divisors(0), 0);
    assert_eq!(sum_proper_divisors(12), 16);
}

#[test]
fn detects_perfect_numbers() {
    assert!(is_perfect(6));
    assert!(is_perfect(28));
    assert!(!is_perfect(1));
    assert!(!is_perfect(12));
}
