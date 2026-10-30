use config::{parse_config, ConfigError};

#[test]
fn comments_and_blank_lines_are_skipped() {
    let entries = parse_config("# top\n\n  # indented\na=1\n").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].key, "a");
}

#[test]
fn empty_keys_are_rejected() {
    assert!(matches!(
        parse_config(" = value\n"),
        Err(ConfigError::EmptyKey(_))
    ));
    assert!(matches!(
        parse_config("=value\n"),
        Err(ConfigError::EmptyKey(_))
    ));
}

#[test]
fn missing_equals_is_rejected() {
    assert!(matches!(
        parse_config("novalue\n"),
        Err(ConfigError::MissingEquals(_))
    ));
}

#[test]
fn duplicate_keys_are_rejected() {
    assert_eq!(
        parse_config("a=1\na=2\n"),
        Err(ConfigError::DuplicateKey("a".to_string()))
    );
}

#[test]
fn trims_keys_and_values() {
    let entries = parse_config("  name  =  eix  \n").unwrap();
    assert_eq!(entries[0].key, "name");
    assert_eq!(entries[0].value, "eix");
}
