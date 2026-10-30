pub fn transpose(rows: &[Vec<i64>]) -> Vec<Vec<i64>> {
    (0..rows[0].len())
        .map(|c| rows.iter().map(|row| row[c]).collect())
        .collect()
}

pub fn multiply(a: &[Vec<i64>], b: &[Vec<i64>]) -> Option<Vec<Vec<i64>>> {
    let mut out = vec![vec![0; b[0].len()]; a.len()];
    for i in 0..a.len() {
        for j in 0..b[0].len() {
            for k in 0..a[0].len() {
                out[i][j] += a[i][k] * b[k][j];
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transposes_2x3() {
        assert_eq!(
            transpose(&[vec![1, 2, 3], vec![4, 5, 6]]),
            vec![vec![1, 4], vec![2, 5], vec![3, 6]]
        );
    }
}
