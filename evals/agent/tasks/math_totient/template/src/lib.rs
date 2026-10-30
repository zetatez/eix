pub fn phi(n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phi_of_one() {
        assert_eq!(phi(1), Some(1));
    }
}
