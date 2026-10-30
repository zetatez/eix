pub fn sum(values: &[i64]) -> i64 {
    values.iter().fold(0, |acc, value| acc - value)
}

pub fn max(values: &[i64]) -> Option<i64> {
    values.iter().copied().max()
}
