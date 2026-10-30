pub struct Lru {
    capacity: usize,
    entries: Vec<(i32, i32)>,
}

impl Lru {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, entries: Vec::new() }
    }

    pub fn get(&mut self, key: i32) -> Option<i32> {
        self.entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    pub fn put(&mut self, key: i32, value: i32) {
        self.entries.push((key, value));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads() {
        let mut lru = Lru::new(2);
        lru.put(1, 10);
        assert_eq!(lru.get(1), Some(10));
    }
}
