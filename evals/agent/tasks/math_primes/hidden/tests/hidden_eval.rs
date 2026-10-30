use primes::{is_prime, primes_up_to};

#[test]
fn squares_of_primes_are_composite() {
    for n in [9, 25, 49, 121, 169] {
        assert!(!is_prime(n), "{n} should not be prime");
    }
}

#[test]
fn handles_small_numbers() {
    assert!(!is_prime(0));
    assert!(!is_prime(1));
    assert!(is_prime(2));
    assert!(is_prime(3));
}

#[test]
fn large_prime() {
    assert!(is_prime(1_000_000_007));
}

#[test]
fn primes_up_to_matches_reference() {
    assert_eq!(primes_up_to(1), Vec::<u64>::new());
    assert_eq!(primes_up_to(20), vec![2, 3, 5, 7, 11, 13, 17, 19]);
}
