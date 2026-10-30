use lru_small::Lru;

#[test]
fn evicts_least_recently_used() {
    let mut lru = Lru::new(2);
    lru.put(1, 10);
    lru.put(2, 20);
    assert_eq!(lru.get(1), Some(10));
    lru.put(3, 30);
    assert_eq!(lru.get(2), None);
    assert_eq!(lru.get(1), Some(10));
    assert_eq!(lru.get(3), Some(30));
}

#[test]
fn updates_value_and_recency() {
    let mut lru = Lru::new(2);
    lru.put(1, 10);
    lru.put(2, 20);
    lru.put(1, 11);
    lru.put(3, 30);
    assert_eq!(lru.get(2), None);
    assert_eq!(lru.get(1), Some(11));
}

#[test]
fn zero_capacity_stores_nothing() {
    let mut lru = Lru::new(0);
    lru.put(1, 10);
    assert_eq!(lru.len(), 0);
    assert_eq!(lru.get(1), None);
}
