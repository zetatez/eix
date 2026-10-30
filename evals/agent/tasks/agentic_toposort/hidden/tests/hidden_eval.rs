use toposort::order;

#[test]
fn sorts_dependencies() {
    assert_eq!(order(4, &[(0, 1), (1, 2), (2, 3)]), Some(vec![0, 1, 2, 3]));
    assert_eq!(order(3, &[(0, 2), (1, 2)]), Some(vec![0, 1, 2]));
    assert_eq!(order(0, &[]), Some(vec![]));
}

#[test]
fn detects_cycles_and_bad_edges() {
    assert_eq!(order(2, &[(0, 1), (1, 0)]), None);
    assert_eq!(order(1, &[(0, 0)]), None);
    assert_eq!(order(2, &[(0, 2)]), None);
    assert_eq!(order(1, &[(1, 0)]), None);
}

#[test]
fn ignores_duplicate_edges() {
    assert_eq!(order(3, &[(0, 1), (0, 1), (1, 2)]), Some(vec![0, 1, 2]));
}
