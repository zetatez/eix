use matrix::{multiply, transpose};

#[test]
fn transposes() {
    assert_eq!(
        transpose(&[vec![1, 2, 3], vec![4, 5, 6]]),
        vec![vec![1, 4], vec![2, 5], vec![3, 6]]
    );
    assert_eq!(transpose(&[]), Vec::<Vec<i64>>::new());
    assert_eq!(transpose(&[vec![]]), Vec::<Vec<i64>>::new());
}

#[test]
fn multiplies() {
    let a = vec![vec![1, 2, 3], vec![4, 5, 6]];
    let b = vec![vec![7, 8], vec![9, 10], vec![11, 12]];
    assert_eq!(multiply(&a, &b), Some(vec![vec![58, 64], vec![139, 154]]));
}

#[test]
fn rejects_bad_shapes_and_overflow() {
    assert_eq!(multiply(&[vec![1, 2]], &[vec![1, 2]]), None);
    assert_eq!(multiply(&[vec![i64::MAX]], &[vec![2]]), None);
    assert_eq!(multiply(&[], &[]), Some(Vec::new()));
}
