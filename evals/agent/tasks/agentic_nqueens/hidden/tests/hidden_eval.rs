use nqueens::count;

#[test]
fn small_boards() {
    assert_eq!(count(0), 1);
    assert_eq!(count(1), 1);
    assert_eq!(count(2), 0);
    assert_eq!(count(3), 0);
    assert_eq!(count(4), 2);
}

#[test]
fn classic_board_sizes() {
    assert_eq!(count(8), 92);
    assert_eq!(count(9), 352);
}
