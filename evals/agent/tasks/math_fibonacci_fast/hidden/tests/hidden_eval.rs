use fibonacci_fast::fib_mod;

#[test]
fn matches_known_values() {
    assert_eq!(fib_mod(0, 1000), Some(0));
    assert_eq!(fib_mod(1, 1000), Some(1));
    assert_eq!(fib_mod(10, 1000), Some(55));
    assert_eq!(fib_mod(30, 1_000_000), Some(832040));
    assert_eq!(fib_mod(90, 1_000_000_007), Some(210345902));
}

#[test]
fn scales_to_a_billion() {
    assert_eq!(fib_mod(1_000_000_000, 1_000_000_007), Some(21));
    assert_eq!(fib_mod(1_000_000_000, 97), Some(82));
}

#[test]
fn handles_zero_modulus() {
    assert_eq!(fib_mod(5, 0), None);
}
