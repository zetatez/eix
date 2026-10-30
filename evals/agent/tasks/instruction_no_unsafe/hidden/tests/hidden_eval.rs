use instr_abs::checked_abs;

#[test]
fn normal_values() {
    assert_eq!(checked_abs(-3), Some(3));
    assert_eq!(checked_abs(0), Some(0));
    assert_eq!(checked_abs(7), Some(7));
}

#[test]
fn min_is_none() {
    assert_eq!(checked_abs(i64::MIN), None);
}
