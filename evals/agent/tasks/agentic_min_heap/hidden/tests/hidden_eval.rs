use min_heap::MinHeap;

#[test]
fn pops_in_ascending_order() {
    let mut heap = MinHeap::new();
    for value in [3, 1, 2, 5, 4] {
        heap.push(value);
    }
    let mut out = Vec::new();
    while let Some(value) = heap.pop() {
        out.push(value);
    }
    assert_eq!(out, vec![1, 2, 3, 4, 5]);
}

#[test]
fn handles_duplicates_and_empty() {
    let mut heap = MinHeap::new();
    assert_eq!(heap.pop(), None);
    heap.push(2);
    heap.push(2);
    heap.push(1);
    assert_eq!(heap.len(), 3);
    assert_eq!(heap.pop(), Some(1));
    assert_eq!(heap.pop(), Some(2));
    assert_eq!(heap.pop(), Some(2));
    assert_eq!(heap.pop(), None);
}
