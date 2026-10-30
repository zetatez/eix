use json_escape::{escape, unescape};

#[test]
fn escapes_specials() {
    assert_eq!(escape("a\"b\\c\nd\te"), "a\\\"b\\\\c\\nd\\te");
    assert_eq!(escape("\u{1}"), "\\u0001");
    assert_eq!(escape("plain"), "plain");
}

#[test]
fn unescapes_specials() {
    assert_eq!(unescape("a\\\"b\\\\c\\nd"), Some("a\"b\\c\nd".to_string()));
    assert_eq!(unescape("\\u0001"), Some("\u{1}".to_string()));
    assert_eq!(unescape("plain"), Some("plain".to_string()));
}

#[test]
fn rejects_bad_escapes() {
    assert_eq!(unescape("\\q"), None);
    assert_eq!(unescape("abc\\"), None);
    assert_eq!(unescape("\\u12"), None);
    assert_eq!(unescape("\\uZZZZ"), None);
}
