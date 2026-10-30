use modpow::mod_pow;

#[test]
fn basic_cases() {
    assert_eq!(mod_pow(2, 10, 1000), 24);
    assert_eq!(mod_pow(2, 0, 5), 1);
    assert_eq!(mod_pow(0, 0, 5), 1);
    assert_eq!(mod_pow(5, 0, 1), 0);
    assert_eq!(mod_pow(3, 4, 1), 0);
}

#[test]
fn large_exponent() {
    assert_eq!(mod_pow(2, 100, 1_000_000_007), 976371285);
    assert_eq!(mod_pow(7, 128, 13), 3);
}

#[test]
#[should_panic(expected = "modulus")]
fn zero_modulus_panics() {
    let _ = mod_pow(1, 1, 0);
}
