use matrix_pow::power;

#[test]
fn multiplies_small_powers() {
    assert_eq!(
        power([[1, 1], [1, 0]], 10, 1000),
        Some([[89, 55], [55, 34]])
    );
    assert_eq!(
        power([[2, 1], [1, 3]], 5, 97),
        Some([[78, 81], [81, 62]])
    );
}

#[test]
fn handles_identity_and_modulus_one() {
    assert_eq!(power([[1, 2], [3, 4]], 0, 7), Some([[1, 0], [0, 1]]));
    assert_eq!(power([[5, 6], [7, 8]], 9, 1), Some([[0, 0], [0, 0]]));
    assert_eq!(power([[1, 1], [1, 0]], 5, 0), None);
}

#[test]
fn scales_to_a_huge_exponent() {
    assert_eq!(
        power([[1, 1], [1, 0]], 1_000_000_000_000_000_000, 1_000_000_007),
        Some([[680_057_396, 209_783_453], [209_783_453, 470_273_943]])
    );
}
