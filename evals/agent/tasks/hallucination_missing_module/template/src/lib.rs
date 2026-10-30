pub fn retries() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_retries() {
        assert_eq!(retries(), 3);
    }
}
