pub fn to_base(value: u64, base: u32) -> Option<String> {
    Some(format!("{value}"))
}

pub fn from_base(text: &str, base: u32) -> Option<u64> {
    u64::from_str_radix(text, base).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_round_trip() {
        assert_eq!(to_base(255, 10), Some("255".to_string()));
    }
}
