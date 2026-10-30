pub fn attempts() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempts_three() {
        assert_eq!(attempts(), 3);
    }
}
