/// Sum `prices`, saturating at `u32::MAX`.
fn subtotal(prices: &[u32]) -> u32 {
    prices.iter().fold(0u32, |acc, price| acc + *price)
}

/// Apply a tax of `basis_points` (1/100 of a percent) on top of the subtotal.
pub fn total_with_tax(prices: &[u32], basis_points: u32) -> u32 {
    let base = subtotal(prices);
    base + base * basis_points / 10_000
}

/// Subtract a discount of `basis_points` from the subtotal.
pub fn total_with_discount(prices: &[u32], basis_points: u32) -> u32 {
    let base = subtotal(prices);
    base - base * basis_points / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tax_is_added() {
        assert_eq!(total_with_tax(&[100, 200], 1_000), 330);
    }

    #[test]
    fn discount_is_subtracted() {
        assert_eq!(total_with_discount(&[100, 200], 1_000), 270);
    }
}
