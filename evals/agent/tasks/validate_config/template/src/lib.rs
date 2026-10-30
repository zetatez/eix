mod parser;

pub use parser::{parse_config, ConfigError, Entry};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_entries() {
        let entries = parse_config("name = eix\n\nvalue=42\n").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].key, "name");
        assert_eq!(entries[0].value, "eix");
        assert_eq!(entries[1].value, "42");
    }
}
