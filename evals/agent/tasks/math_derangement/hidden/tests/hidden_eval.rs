use derangement::derangements;

#[test]
fn matches_known_values() {
    assert_eq!(derangements(0), Some(1));
    assert_eq!(derangements(1), Some(0));
    assert_eq!(derangements(2), Some(1));
    assert_eq!(derangements(5), Some(44));
    assert_eq!(derangements(10), Some(1_334_961));
}

#[test]
fn largest_fitting_value() {
    assert_eq!(derangements(20), Some(895_014_631_192_902_121));
}

#[test]
fn reports_overflow() {
    assert_eq!(derangements(21), None);
}
