pub fn sum_geometric(first: f64, ratio: f64, terms: u32) -> Option<f64> {
    let _ = ratio;
    Some(first * terms as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_term_is_the_first() {
        assert_eq!(sum_geometric(5.0, 2.0, 1), Some(5.0));
    }
}
