pub fn read(path: &str) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_errors() {
        assert!(read("/definitely/not/here").is_err());
    }
}
