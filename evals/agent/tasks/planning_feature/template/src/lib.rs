pub fn dedupe(values: &[i32]) -> Vec<i32> {
    values.to_vec()
}

pub fn unique_sorted(values: &[i32]) -> Vec<i32> {
    values.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes() {
        assert_eq!(dedupe(&[1, 1, 2]), vec![1, 2]);
    }
}
