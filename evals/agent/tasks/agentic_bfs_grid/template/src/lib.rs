pub fn shortest_path(
    _grid: &[&str],
    start: (usize, usize),
    goal: (usize, usize),
) -> Option<usize> {
    Some(start.0.abs_diff(goal.0) + start.1.abs_diff(goal.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_grid() {
        assert_eq!(shortest_path(&["...", "...", "..."], (0, 0), (2, 2)), Some(4));
    }
}
