use slugify::slugify;

#[test]
fn keeps_words_and_numbers() {
    assert_eq!(slugify("Hello, World!"), "hello-world");
    assert_eq!(slugify("  Rust 2021  "), "rust-2021");
    assert_eq!(slugify("a---b"), "a-b");
}

#[test]
fn empty_and_non_ascii() {
    assert_eq!(slugify(""), "");
    assert_eq!(slugify("---"), "");
    assert_eq!(slugify("Café au lait"), "caf-au-lait");
}
