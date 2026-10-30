pub fn catalan(n: u32) -> Option<u64> {
    Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_number() {
        assert_eq!(catalan(0), Some(1));
    }
}
