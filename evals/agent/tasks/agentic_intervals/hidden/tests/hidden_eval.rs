use intervals::merge;

#[test]
fn merges_overlaps_and_touching() {
    assert_eq!(merge(&[(1, 3), (2, 6), (8, 10)]), vec![(1, 6), (8, 10)]);
    assert_eq!(merge(&[(5, 7), (1, 5)]), vec![(1, 7)]);
}

#[test]
fn handles_unsorted_input() {
    assert_eq!(merge(&[(8, 10), (1, 3), (2, 6)]), vec![(1, 6), (8, 10)]);
}

#[test]
fn empty_and_single() {
    assert_eq!(merge(&[]), Vec::new());
    assert_eq!(merge(&[(1, 1)]), vec![(1, 1)]);
}
