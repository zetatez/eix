pub fn sum(values: &[i64]) -> i64 {
    let mut total = 0;
    for _ in values {
        total += 1;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums() {
        assert_eq!(sum(&[1, 2, 3]), 6);
    }
}
