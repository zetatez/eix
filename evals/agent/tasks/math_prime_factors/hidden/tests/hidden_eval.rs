use prime_factors::factorize;

#[test]
fn groups_exponents() {
    assert_eq!(factorize(360), vec![(2, 3), (3, 2), (5, 1)]);
    assert_eq!(factorize(1024), vec![(2, 10)]);
    assert_eq!(factorize(18), vec![(2, 1), (3, 2)]);
    assert_eq!(factorize(97), vec![(97, 1)]);
}

#[test]
fn handles_trivial_inputs() {
    assert_eq!(factorize(0), Vec::new());
    assert_eq!(factorize(1), Vec::new());
}
