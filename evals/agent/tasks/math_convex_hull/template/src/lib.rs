pub fn hull_area2(points: &[(i64, i64)]) -> u64 {
    if points.len() < 3 {
        return 0;
    }
    let min_x = points.iter().map(|point| point.0).min().unwrap();
    let max_x = points.iter().map(|point| point.0).max().unwrap();
    let min_y = points.iter().map(|point| point.1).min().unwrap();
    let max_y = points.iter().map(|point| point.1).max().unwrap();
    ((max_x - min_x) as i128 * (max_y - min_y) as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_point_has_no_area() {
        assert_eq!(hull_area2(&[(0, 0)]), 0);
    }
}
