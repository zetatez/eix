use wordwrap::wrap;

#[test]
fn wraps_at_spaces() {
    assert_eq!(wrap("the quick brown fox", 10), vec!["the quick", "brown fox"]);
    assert_eq!(wrap("a b c", 1), vec!["a", "b", "c"]);
}

#[test]
fn normalizes_whitespace() {
    assert_eq!(wrap("  a   b  c ", 5), vec!["a b c"]);
    assert_eq!(wrap("", 5), Vec::<String>::new());
}

#[test]
fn long_word_is_not_split() {
    assert_eq!(wrap("supercalifragilistic", 5), vec!["supercalifragilistic"]);
}

#[test]
fn exact_fit_stays_on_one_line() {
    assert_eq!(wrap("abc def", 7), vec!["abc def"]);
}
