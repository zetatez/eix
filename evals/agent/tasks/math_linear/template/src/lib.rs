pub fn solve(a: f64, b: f64, c: f64, d: f64, e: f64, f: f64) -> Option<(f64, f64)> {
    let det = a * d - b * c;
    Some(((e * d - b * f) / det, (a * f - e * c) / det))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solves_simple_system() {
        assert_eq!(solve(1.0, 0.0, 0.0, 1.0, 2.0, 3.0), Some((2.0, 3.0)));
    }
}
