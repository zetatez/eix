use union_find::DisjointSet;

#[test]
fn merges_transitively() {
    let mut set = DisjointSet::new(5);
    assert!(set.union(0, 1));
    assert!(set.union(1, 2));
    assert_eq!(set.find(0), set.find(2));
    assert_eq!(set.components(), 3);
}

#[test]
fn repeated_union_is_false() {
    let mut set = DisjointSet::new(2);
    assert!(set.union(0, 1));
    assert!(!set.union(0, 1));
    assert!(!set.union(1, 0));
    assert_eq!(set.components(), 1);
}

#[test]
fn empty_set() {
    let set = DisjointSet::new(0);
    assert_eq!(set.components(), 0);
}
