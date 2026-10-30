use binomial_mod::binom_mod;

const P: u64 = 1_000_000_007;

#[test]
fn computes_small_binomials() {
    assert_eq!(binom_mod(10, 5, P), Some(252));
    assert_eq!(binom_mod(5, 6, P), Some(0));
    assert_eq!(binom_mod(10, 5, 13), Some(5));
}

#[test]
fn handles_large_n_with_lucas() {
    assert_eq!(binom_mod(1000, 500, P), Some(159_835_829));
    assert_eq!(binom_mod(2 * P + 5, P + 3, P), Some(20));
}

#[test]
fn rejects_invalid_modulus() {
    assert_eq!(binom_mod(10, 5, 0), None);
    assert_eq!(binom_mod(10, 5, 1), None);
}
