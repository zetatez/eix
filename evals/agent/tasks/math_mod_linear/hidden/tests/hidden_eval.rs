use mod_linear::solve_linear;

#[test]
fn solves_congruences() {
    assert_eq!(solve_linear(4, 2, 6), Some(2));
    assert_eq!(solve_linear(3, 1, 7), Some(5));
    assert_eq!(solve_linear(10, 5, 15), Some(2));
    assert_eq!(solve_linear(7, 3, 11), Some(2));
    assert_eq!(solve_linear(-3, 1, 7), Some(2));
}

#[test]
fn detects_impossible_equations() {
    assert_eq!(solve_linear(2, 1, 4), None);
    assert_eq!(solve_linear(0, 1, 5), None);
    assert_eq!(solve_linear(4, 1, 6), None);
    assert_eq!(solve_linear(3, 1, 0), None);
}

#[test]
fn handles_zero_coefficient() {
    assert_eq!(solve_linear(0, 0, 5), Some(0));
    assert_eq!(solve_linear(0, 5, 5), Some(0));
}
