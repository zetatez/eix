use coin_change::min_coins;

#[test]
fn finds_non_greedy_optima() {
    assert_eq!(min_coins(&[1, 3, 4], 6), Some(2));
    assert_eq!(min_coins(&[1, 5, 6, 9], 11), Some(2));
}

#[test]
fn handles_impossible_and_trivial() {
    assert_eq!(min_coins(&[2], 3), None);
    assert_eq!(min_coins(&[], 5), None);
    assert_eq!(min_coins(&[1, 2], 0), Some(0));
    assert_eq!(min_coins(&[0, 2], 4), None);
}
