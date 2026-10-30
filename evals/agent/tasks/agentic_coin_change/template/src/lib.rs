pub fn min_coins(coins: &[u32], amount: u32) -> Option<u32> {
    if coins.contains(&0) {
        return None;
    }
    let mut sorted = coins.to_vec();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    let mut remaining = amount;
    let mut count = 0;
    for coin in sorted {
        count += remaining / coin;
        remaining %= coin;
    }
    if remaining == 0 { Some(count) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_coin_exact() {
        assert_eq!(min_coins(&[5], 5), Some(1));
    }
}
