use spiral::spiral;

#[test]
fn walks_a_square() {
    let matrix = vec![vec![1, 2, 3], vec![4, 5, 6], vec![7, 8, 9]];
    assert_eq!(spiral(&matrix), vec![1, 2, 3, 6, 9, 8, 7, 4, 5]);
}

#[test]
fn walks_a_rectangle() {
    let matrix = vec![
        vec![1, 2, 3, 4],
        vec![5, 6, 7, 8],
        vec![9, 10, 11, 12],
    ];
    assert_eq!(spiral(&matrix), vec![1, 2, 3, 4, 8, 12, 11, 10, 9, 5, 6, 7]);
}

#[test]
fn rejects_empty_and_ragged() {
    assert_eq!(spiral(&[]), Vec::<i32>::new());
    assert_eq!(spiral(&[vec![1, 2], vec![3]]), Vec::<i32>::new());
}
