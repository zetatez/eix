use trap_rain::trap;

#[test]
fn traps_the_classic_profile() {
    let heights = [0, 1, 0, 2, 1, 0, 1, 3, 2, 1, 2, 1];
    assert_eq!(trap(&heights), 6);
    assert_eq!(trap(&[4, 2, 3]), 1);
    assert_eq!(trap(&[5, 0, 5]), 5);
}

#[test]
fn handles_short_input() {
    assert_eq!(trap(&[]), 0);
    assert_eq!(trap(&[5]), 0);
    assert_eq!(trap(&[5, 1]), 0);
}
