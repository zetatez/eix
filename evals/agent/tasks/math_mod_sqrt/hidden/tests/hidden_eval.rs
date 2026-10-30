use mod_sqrt::sqrt_mod;

#[test]
fn finds_small_roots() {
    assert_eq!(sqrt_mod(0, 13), Some(0));
    assert_eq!(sqrt_mod(1, 13), Some(1));
    assert_eq!(sqrt_mod(4, 7), Some(2));
    assert_eq!(sqrt_mod(10, 13), Some(6));
    assert_eq!(sqrt_mod(2, 7), Some(3));
    assert_eq!(sqrt_mod(5, 11), Some(4));
}

#[test]
fn rejects_non_residues_and_bad_moduli() {
    assert_eq!(sqrt_mod(3, 7), None);
    assert_eq!(sqrt_mod(5, 13), None);
    assert_eq!(sqrt_mod(2, 2), None);
    assert_eq!(sqrt_mod(2, 1), None);
}

#[test]
fn works_for_a_large_prime() {
    assert_eq!(sqrt_mod(2, 1_000_000_007), Some(59_713_600));
}
