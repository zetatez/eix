use knapsack::max_value;

#[test]
fn beats_greedy_by_value() {
    assert_eq!(max_value(&[2, 3, 4], &[3, 4, 5], 5), Some(7));
    assert_eq!(max_value(&[1, 3, 4, 5], &[1, 4, 5, 7], 7), Some(9));
}

#[test]
fn handles_edges() {
    assert_eq!(max_value(&[], &[], 10), Some(0));
    assert_eq!(max_value(&[5], &[9], 0), Some(0));
    assert_eq!(max_value(&[1], &[], 3), None);
}
