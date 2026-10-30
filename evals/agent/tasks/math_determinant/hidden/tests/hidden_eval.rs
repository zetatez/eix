use determinant::{det3, invertible};

#[test]
fn computes_determinants() {
    assert_eq!(det3([[1, 2, 3], [4, 5, 6], [7, 8, 10]]), Some(-3));
    assert_eq!(det3([[2, 0, 0], [0, 3, 0], [0, 0, 4]]), Some(24));
    assert!(!invertible([[1, 2, 3], [4, 5, 6], [7, 8, 9]]));
    assert!(invertible([[1, 0, 0], [0, 1, 0], [0, 0, 1]]));
}

#[test]
fn reports_overflow() {
    assert_eq!(
        det3([[i64::MAX, 0, 0], [0, 2, 0], [0, 0, 1]]),
        None
    );
    assert!(!invertible([[i64::MAX, 0, 0], [0, 2, 0], [0, 0, 1]]));
}
