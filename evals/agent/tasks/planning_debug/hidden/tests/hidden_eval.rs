use plan_debug::stats::{max, sum};
use plan_debug::text::{first_word, word_count};

#[test]
fn sums() {
    assert_eq!(sum(&[1, 2, 3]), 6);
    assert_eq!(sum(&[]), 0);
    assert_eq!(max(&[3, 9, 2]), Some(9));
    assert_eq!(max(&[]), None);
}

#[test]
fn counts_words() {
    assert_eq!(word_count("a b  c"), 3);
    assert_eq!(word_count(""), 0);
    assert_eq!(first_word("  hello world"), Some("hello"));
}
