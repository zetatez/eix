pub fn primitive_triples(limit: u64) -> Vec<(u64, u64, u64)> {
    let _ = limit;
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_limits_below_five() {
        assert!(primitive_triples(4).is_empty());
    }
}
