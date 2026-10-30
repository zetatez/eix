use kadane::max_subarray;

#[test]
fn finds_the_best_subarray() {
    let values = [-2, 1, -3, 4, -1, 2, 1, -5, 4];
    assert_eq!(max_subarray(&values), Some((6, 3, 6)));
    assert_eq!(max_subarray(&[5, -10, 5]), Some((5, 0, 0)));
}

#[test]
fn handles_all_negative_and_empty() {
    assert_eq!(max_subarray(&[-3, -1, -2]), Some((-1, 1, 1)));
    assert_eq!(max_subarray(&[]), None);
    assert_eq!(max_subarray(&[1]), Some((1, 0, 0)));
}
