use plan_feature::{dedupe, unique_sorted};

#[test]
fn dedupe_keeps_first_occurrence() {
    assert_eq!(dedupe(&[1, 1, 2, 1, 3]), vec![1, 2, 3]);
    assert_eq!(dedupe(&[]), Vec::new());
}

#[test]
fn unique_sorted_sorts() {
    assert_eq!(unique_sorted(&[3, 1, 3, 2]), vec![1, 2, 3]);
    assert_eq!(unique_sorted(&[]), Vec::new());
}
