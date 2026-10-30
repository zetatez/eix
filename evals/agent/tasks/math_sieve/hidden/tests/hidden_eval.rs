use sieve::primes_up_to;

#[test]
fn lists_small_primes() {
    assert_eq!(primes_up_to(30), vec![2, 3, 5, 7, 11, 13, 17, 19, 23, 29]);
    assert_eq!(primes_up_to(2), vec![2]);
    assert_eq!(primes_up_to(1), Vec::<u64>::new());
    assert_eq!(primes_up_to(0), Vec::<u64>::new());
}

#[test]
fn counts_larger_ranges() {
    assert_eq!(primes_up_to(100).len(), 25);
    assert_eq!(primes_up_to(100).last(), Some(&97));
    assert_eq!(primes_up_to(10_000).len(), 1229);
}
