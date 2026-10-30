use quickselect::kth_smallest;

#[test]
fn selects_in_any_order() {
    assert_eq!(kth_smallest(&mut [3, 1, 2], 1), Some(2));
    assert_eq!(kth_smallest(&mut [3, 1, 2], 0), Some(1));
    assert_eq!(kth_smallest(&mut [3, 1, 2], 2), Some(3));
}

#[test]
fn handles_duplicates_and_bounds() {
    assert_eq!(kth_smallest(&mut [2, 2, 1], 0), Some(1));
    assert_eq!(kth_smallest(&mut [2, 2, 1], 2), Some(2));
    assert_eq!(kth_smallest(&mut [1], 1), None);
    assert_eq!(kth_smallest(&mut [], 0), None);
}
