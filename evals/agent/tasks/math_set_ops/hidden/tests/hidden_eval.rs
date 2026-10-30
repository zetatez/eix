use set_ops::{difference, intersection, union};

#[test]
fn normalizes_and_sorts() {
    assert_eq!(union(&[3, 1, 2], &[2, 4]), vec![1, 2, 3, 4]);
    assert_eq!(intersection(&[3, 1, 2], &[2, 4]), vec![2]);
    assert_eq!(difference(&[3, 1, 2], &[2]), vec![1, 3]);
}

#[test]
fn removes_duplicates() {
    assert_eq!(union(&[1, 1, 2], &[2, 3, 3]), vec![1, 2, 3]);
    assert_eq!(intersection(&[1, 1, 2, 2], &[2, 2, 3]), vec![2]);
    assert_eq!(difference(&[1, 1, 2, 2], &[2]), vec![1]);
}

#[test]
fn handles_empty_sets() {
    assert_eq!(union(&[], &[]), Vec::<i32>::new());
    assert_eq!(union(&[], &[2, 1]), vec![1, 2]);
    assert_eq!(intersection(&[1, 2], &[]), Vec::<i32>::new());
    assert_eq!(difference(&[], &[1]), Vec::<i32>::new());
}
