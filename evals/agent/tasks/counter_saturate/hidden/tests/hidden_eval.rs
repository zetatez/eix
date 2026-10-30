use counter::Counter;

#[test]
fn add_saturates_at_max() {
    let mut counter = Counter::new(i64::MAX - 1);
    assert_eq!(counter.add(5), i64::MAX);
    assert_eq!(counter.value(), i64::MAX);
    assert_eq!(counter.add(1), i64::MAX);
}

#[test]
fn sub_saturates_at_min() {
    let mut counter = Counter::new(i64::MIN + 1);
    assert_eq!(counter.sub(5), i64::MIN);
    assert_eq!(counter.value(), i64::MIN);
    assert_eq!(counter.sub(1), i64::MIN);
}

#[test]
fn normal_arithmetic_still_works() {
    let mut counter = Counter::new(0);
    assert_eq!(counter.add(-3), -3);
    assert_eq!(counter.sub(-7), 4);
}
