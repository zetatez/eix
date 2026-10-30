pub fn subtotal_a(prices: &[i64]) -> i64 {
    let mut total = 0;
    for price in prices {
        total += *price;
    }
    total
}

pub fn subtotal_b(prices: &[i64]) -> i64 {
    let mut total = 0;
    for price in prices {
        total += *price;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_prices() {
        assert_eq!(subtotal_a(&[1, 2, 3]), 6);
        assert_eq!(subtotal_b(&[4, 5]), 9);
    }
}
