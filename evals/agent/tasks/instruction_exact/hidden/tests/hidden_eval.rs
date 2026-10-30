use instr::{checked_div, sum_checked};

#[test]
fn empty_input_is_an_error() {
    assert_eq!(sum_checked(&[]), Err("empty"));
}

#[test]
fn sums_normal_values() {
    assert_eq!(sum_checked(&[1, 2, 3]), Ok(6));
    assert_eq!(sum_checked(&[-5, 10, -2]), Ok(3));
    assert_eq!(sum_checked(&[0]), Ok(0));
}

#[test]
fn overflow_is_reported_without_panicking() {
    assert_eq!(sum_checked(&[i64::MAX, 1]), Err("overflow"));
    assert_eq!(sum_checked(&[i64::MIN, -1]), Err("overflow"));
    assert_eq!(sum_checked(&[i64::MAX, i64::MIN]), Ok(-1));
}

#[test]
fn existing_api_is_unchanged() {
    assert_eq!(checked_div(8, 2), Ok(4));
    assert_eq!(checked_div(1, 0), Err("divide by zero"));
}
