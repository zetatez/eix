pub fn format_duration(total_seconds: u64) -> String {
    format!(
        "{}:{}:{}",
        total_seconds / 3600,
        total_seconds / 60,
        total_seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example() {
        assert_eq!(format_duration(3661), "1:01:01");
    }
}
