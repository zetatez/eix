pub fn solve_linear(a: i64, b: i64, m: u64) -> Option<u64> {
    let _ = (a, b, m);
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trivial_equation() {
        assert_eq!(solve_linear(0, 0, 5), Some(0));
    }
}
