use std::collections::HashSet;

#[derive(Default)]
pub struct Trie {
    words: HashSet<String>,
}

impl Trie {
    pub fn new() -> Self {
        Self { words: HashSet::new() }
    }

    pub fn insert(&mut self, word: &str) {
        self.words.insert(word.to_string());
    }

    pub fn contains(&self, word: &str) -> bool {
        self.words.contains(word)
    }

    pub fn starts_with(&self, _prefix: &str) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_exact_words() {
        let mut trie = Trie::new();
        trie.insert("apple");
        assert!(trie.contains("apple"));
    }
}
