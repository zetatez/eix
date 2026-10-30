use collatz::{max_value, steps};

#[test]
fn counts_steps() {
    assert_eq!(steps(1), Some(0));
    assert_eq!(steps(6), Some(8));
    assert_eq!(steps(27), Some(111));
    assert_eq!(steps(0), None);
}

#[test]
fn tracks_the_maximum() {
    assert_eq!(max_value(1), Some(1));
    assert_eq!(max_value(6), Some(16));
    assert_eq!(max_value(0), None);
}

#[test]
fn reports_overflow() {
    assert_eq!(steps(u64::MAX), None);
    assert_eq!(max_value(u64::MAX), None);
}
