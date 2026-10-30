pub fn max_subarray(values: &[i32]) -> Option<(i64, usize, usize)> {
    let (index, value) = values
        .iter()
        .enumerate()
        .max_by_key(|(_, value)| **value)?;
    Some((*value as i64, index, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_value() {
        assert_eq!(max_subarray(&[5]), Some((5, 0, 0)));
    }
}
