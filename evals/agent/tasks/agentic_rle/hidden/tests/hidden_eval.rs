use rle::{decode, encode};

#[test]
fn encodes_and_decodes() {
    assert_eq!(encode("aaabcc"), "a3bc2");
    assert_eq!(encode(""), "");
    assert_eq!(decode("a3bc2"), Some("aaabcc".to_string()));
    assert_eq!(decode("a10"), Some("a".repeat(10)));
    assert_eq!(decode("a"), Some("a".to_string()));
    assert_eq!(decode(""), Some(String::new()));
}

#[test]
fn rejects_malformed_input() {
    assert_eq!(decode("3a"), None);
    assert_eq!(decode("a0"), None);
    assert_eq!(decode("a1b0"), None);
}
