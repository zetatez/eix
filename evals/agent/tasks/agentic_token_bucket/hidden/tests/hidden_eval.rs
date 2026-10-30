use token_bucket::Bucket;

#[test]
fn drains_and_refills() {
    let mut bucket = Bucket::new(2, 1);
    assert!(bucket.try_take(0));
    assert!(bucket.try_take(0));
    assert!(!bucket.try_take(0));
    assert!(bucket.try_take(1));
    assert!(bucket.try_take(3));
    assert!(bucket.try_take(3));
    assert!(!bucket.try_take(3));
}

#[test]
fn refill_is_capped() {
    let mut bucket = Bucket::new(2, 100);
    assert!(bucket.try_take(0));
    assert!(bucket.try_take(1000));
    assert!(bucket.try_take(1000));
    assert!(!bucket.try_take(1000));
}

#[test]
fn zero_rate_never_refills() {
    let mut bucket = Bucket::new(1, 0);
    assert!(bucket.try_take(0));
    assert!(!bucket.try_take(100));
}
