use search_bounds::{lower_bound, upper_bound};

#[test]
fn handles_duplicates() {
    let values = [1, 2, 2, 2, 4];
    assert_eq!(lower_bound(&values, 2), 1);
    assert_eq!(upper_bound(&values, 2), 4);
}

#[test]
fn handles_missing_values() {
    let values = [1, 2, 2, 2, 4];
    assert_eq!(lower_bound(&values, 3), 4);
    assert_eq!(upper_bound(&values, 3), 4);
    assert_eq!(lower_bound(&values, 0), 0);
    assert_eq!(upper_bound(&values, 0), 0);
    assert_eq!(upper_bound(&values, 4), 5);
    assert_eq!(lower_bound(&[], 1), 0);
}
