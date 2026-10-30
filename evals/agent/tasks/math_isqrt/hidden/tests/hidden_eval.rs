use isqrt::{is_perfect_square, isqrt};

#[test]
fn floors_the_square_root() {
    assert_eq!(isqrt(0), 0);
    assert_eq!(isqrt(1), 1);
    assert_eq!(isqrt(15), 3);
    assert_eq!(isqrt(16), 4);
    assert_eq!(isqrt(17), 4);
    assert_eq!(isqrt(1_000_000_000_000_000_000), 1_000_000_000);
    assert_eq!(isqrt(u64::MAX), 4_294_967_295);
}

#[test]
fn detects_perfect_squares() {
    assert!(is_perfect_square(0));
    assert!(is_perfect_square(1));
    assert!(is_perfect_square(16));
    assert!(!is_perfect_square(15));
    assert!(!is_perfect_square(u64::MAX));
}
