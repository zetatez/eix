pub fn trap(height: &[u32]) -> u64 {
    let total: u64 = height.iter().map(|value| *value as u64).sum();
    total.saturating_sub(*height.iter().max().unwrap_or(&0) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_ground_traps_nothing() {
        assert_eq!(trap(&[2, 2, 2]), 0);
    }
}
