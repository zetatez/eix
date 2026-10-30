use merge_inversions::sort_and_count;

#[test]
fn counts_inversions() {
    assert_eq!(sort_and_count(&[2, 4, 1, 3]), (vec![1, 2, 3, 4], 3));
    assert_eq!(sort_and_count(&[3, 2, 1]), (vec![1, 2, 3], 3));
    assert_eq!(sort_and_count(&[1, 1, 1]), (vec![1, 1, 1], 0));
}

#[test]
fn handles_empty_and_single() {
    assert_eq!(sort_and_count(&[]), (Vec::new(), 0));
    assert_eq!(sort_and_count(&[9]), (vec![9], 0));
}
