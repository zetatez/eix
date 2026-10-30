pub fn max_value(weights: &[u64], values: &[u64], capacity: u64) -> Option<u64> {
    if weights.len() != values.len() {
        return None;
    }
    let mut items: Vec<(u64, u64)> = weights.iter().copied().zip(values.iter().copied()).collect();
    items.sort_unstable_by(|a, b| b.1.cmp(&a.1));
    let mut remaining = capacity;
    let mut total = 0u64;
    for (weight, value) in items {
        if weight <= remaining {
            remaining -= weight;
            total += value;
        }
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_a_single_item() {
        assert_eq!(max_value(&[2], &[5], 3), Some(5));
    }
}
