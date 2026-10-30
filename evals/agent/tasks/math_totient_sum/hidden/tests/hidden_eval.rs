use totient_sum::sum_phi;

#[test]
fn matches_known_prefix_sums() {
    assert_eq!(sum_phi(0), 0);
    assert_eq!(sum_phi(1), 1);
    assert_eq!(sum_phi(2), 2);
    assert_eq!(sum_phi(10), 32);
    assert_eq!(sum_phi(100), 3044);
    assert_eq!(sum_phi(1000), 304_192);
}

#[test]
fn scales_to_a_million() {
    assert_eq!(sum_phi(1_000_000), 303_963_552_392);
}
