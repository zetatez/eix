pub fn spiral(matrix: &[Vec<i32>]) -> Vec<i32> {
    matrix.iter().flatten().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_cell() {
        assert_eq!(spiral(&[vec![1]]), vec![1]);
    }
}
