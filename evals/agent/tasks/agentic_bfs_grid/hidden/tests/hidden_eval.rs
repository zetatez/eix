use bfs_grid::shortest_path;

#[test]
fn walks_around_walls() {
    let grid = [".....", ".###.", "....."];
    assert_eq!(shortest_path(&grid, (1, 0), (1, 4)), Some(6));
}

#[test]
fn detects_unreachable_goals() {
    let grid = ["..#", "###", "..."];
    assert_eq!(shortest_path(&grid, (0, 0), (2, 2)), None);
}

#[test]
fn handles_trivial_and_invalid_input() {
    assert_eq!(shortest_path(&["."], (0, 0), (0, 0)), Some(0));
    assert_eq!(shortest_path(&["."], (0, 0), (1, 0)), None);
    assert_eq!(shortest_path(&["#"], (0, 0), (0, 0)), None);
    assert_eq!(shortest_path(&[], (0, 0), (0, 0)), None);
}
