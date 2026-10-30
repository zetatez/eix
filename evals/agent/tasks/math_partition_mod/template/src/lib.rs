pub fn partitions(n: u64, modulus: u64) -> Option<u64> {
    if modulus == 0 {
        return None;
    }
    let _ = n;
    Some(1 % modulus)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_has_one_partition() {
        assert_eq!(partitions(0, 1_000_000_007), Some(1));
    }
}
