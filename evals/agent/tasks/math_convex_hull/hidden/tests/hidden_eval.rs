use convex_hull::hull_area2;

#[test]
fn computes_twice_the_area() {
    assert_eq!(hull_area2(&[(0, 0), (4, 0), (4, 3)]), 12);
    assert_eq!(hull_area2(&[(0, 0), (1, 0), (0, 1)]), 1);
    assert_eq!(hull_area2(&[(0, 0), (1, 0), (1, 1), (0, 1)]), 2);
}

#[test]
fn ignores_interior_and_collinear_points() {
    assert_eq!(
        hull_area2(&[(0, 0), (2, 0), (1, 1), (0, 2), (2, 2)]),
        8
    );
    assert_eq!(hull_area2(&[(0, 0), (1, 0), (2, 0), (3, 0)]), 0);
    assert_eq!(hull_area2(&[(0, 0), (1, 1), (2, 2)]), 0);
}

#[test]
fn handles_duplicates_negatives_and_small_inputs() {
    assert_eq!(hull_area2(&[(0, 0), (0, 0), (1, 0), (0, 1)]), 1);
    assert_eq!(hull_area2(&[(-2, -2), (2, -2), (2, 2), (-2, 2)]), 32);
    assert_eq!(hull_area2(&[]), 0);
    assert_eq!(hull_area2(&[(5, 5)]), 0);
}
