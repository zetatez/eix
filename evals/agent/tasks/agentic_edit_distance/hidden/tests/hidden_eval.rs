use edit_distance::distance;

#[test]
fn computes_classic_distances() {
    assert_eq!(distance("kitten", "sitting"), 3);
    assert_eq!(distance("flaw", "lawn"), 2);
    assert_eq!(distance("", "abc"), 3);
    assert_eq!(distance("abc", ""), 3);
}

#[test]
fn works_on_characters_not_bytes() {
    assert_eq!(distance("é", "e"), 1);
    assert_eq!(distance("日本語", "日本"), 1);
    assert_eq!(distance("ab", "ba"), 2);
}
