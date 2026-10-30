pub const DEFAULT_LIMIT: u32 = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_limit() {
        assert_eq!(DEFAULT_LIMIT, 1024);
    }
}
