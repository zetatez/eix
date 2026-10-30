use globmatch::matches;

#[test]
fn handles_star_and_question() {
    assert!(matches("a*b", "ab"));
    assert!(matches("a*b", "acb"));
    assert!(matches("a?c", "abc"));
    assert!(!matches("a?c", "ac"));
    assert!(matches("a**b", "ab"));
    assert!(matches("*", ""));
    assert!(matches("*", "abc"));
}

#[test]
fn requires_a_full_match() {
    assert!(matches("", ""));
    assert!(!matches("", "a"));
    assert!(!matches("abc", "abcd"));
    assert!(matches("ä?", "äö"));
    assert!(!matches("a?", "a"));
}
