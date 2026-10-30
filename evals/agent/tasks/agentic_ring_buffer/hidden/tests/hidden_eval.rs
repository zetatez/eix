use ring_buffer::RingBuffer;

#[test]
fn evicts_oldest_and_keeps_order() {
    let mut buffer = RingBuffer::new(3);
    for value in 1..=5 {
        buffer.push(value);
    }
    assert_eq!(buffer.len(), 3);
    assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![3, 4, 5]);
    buffer.push(6);
    assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![4, 5, 6]);
}

#[test]
fn zero_capacity_stores_nothing() {
    let mut buffer = RingBuffer::new(0);
    buffer.push(1);
    assert_eq!(buffer.len(), 0);
    assert!(buffer.iter().next().is_none());
}
