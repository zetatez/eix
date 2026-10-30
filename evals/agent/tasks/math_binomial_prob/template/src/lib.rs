pub fn binomial_probability(n: u32, k: u32, p: f64) -> Option<f64> {
    if k > n {
        return None;
    }
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impossible_event_is_zero() {
        assert_eq!(binomial_probability(3, 4, 0.5), None);
    }
}
