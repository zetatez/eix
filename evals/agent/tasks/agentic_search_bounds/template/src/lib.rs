pub fn lower_bound(values: &[i32], target: i32) -> usize {
    values.iter().position(|value| *value == target).unwrap_or(values.len())
}

pub fn upper_bound(values: &[i32], target: i32) -> usize {
    values.iter().position(|value| *value == target).unwrap_or(values.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_exact_value() {
        assert_eq!(lower_bound(&[1, 3], 3), 1);
    }
}
