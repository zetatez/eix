use induction_cubes::sum_cubes;

#[test]
fn matches_the_closed_form() {
    assert_eq!(sum_cubes(0), Some(0));
    assert_eq!(sum_cubes(1), Some(1));
    assert_eq!(sum_cubes(2), Some(9));
    assert_eq!(sum_cubes(3), Some(36));
    assert_eq!(sum_cubes(10), Some(3025));
}

#[test]
fn reports_overflow() {
    assert_eq!(sum_cubes(200_000), None);
    assert_eq!(sum_cubes(u64::MAX), None);
}
