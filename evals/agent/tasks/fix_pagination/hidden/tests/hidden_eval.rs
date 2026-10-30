use pagination::paginate;

#[test]
fn empty_pages_do_not_panic() {
    let items = [1, 2, 3, 4, 5];
    assert!(paginate(&items, 0, 2).is_empty());
    assert!(paginate(&items, 99, 2).is_empty());
    assert!(paginate(&items, 1, 0).is_empty());
    assert!(paginate(&[], 1, 10).is_empty());
}

#[test]
fn pages_are_one_based_and_clamped() {
    let items = [1, 2, 3, 4, 5];
    assert_eq!(paginate(&items, 1, 2), &[1, 2]);
    assert_eq!(paginate(&items, 2, 2), &[3, 4]);
    assert_eq!(paginate(&items, 3, 2), &[5]);
    assert_eq!(paginate(&items, 1, 10), &[1, 2, 3, 4, 5]);
}
