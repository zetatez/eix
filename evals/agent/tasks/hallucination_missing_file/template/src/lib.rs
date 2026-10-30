pub fn retry_delay(attempt: u32) -> u64 {
    (1u64 << attempt.min(6)) * 100
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows() {
        assert_eq!(retry_delay(0), 100);
        assert_eq!(retry_delay(3), 800);
    }
}
