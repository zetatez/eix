pub fn primes_up_to(limit: u64) -> Vec<u64> {
    (2..=limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_small_primes() {
        assert!(primes_up_to(3).contains(&2));
    }
}
