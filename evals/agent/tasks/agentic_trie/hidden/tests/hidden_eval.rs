use trie::Trie;

#[test]
fn distinguishes_words_and_prefixes() {
    let mut trie = Trie::new();
    trie.insert("apple");
    trie.insert("app");
    assert!(trie.contains("apple"));
    assert!(trie.contains("app"));
    assert!(!trie.contains("ap"));
    assert!(trie.starts_with("ap"));
    assert!(trie.starts_with("appl"));
    assert!(!trie.starts_with("b"));
}

#[test]
fn handles_empty_strings() {
    let mut trie = Trie::new();
    assert!(trie.starts_with(""));
    assert!(!trie.contains(""));
    trie.insert("");
    assert!(trie.contains(""));
}
