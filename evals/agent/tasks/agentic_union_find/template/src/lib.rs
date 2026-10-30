pub struct DisjointSet {
    parent: Vec<usize>,
}

impl DisjointSet {
    pub fn new(n: usize) -> Self {
        Self { parent: (0..n).collect() }
    }

    pub fn find(&self, x: usize) -> usize {
        self.parent[x]
    }

    pub fn union(&mut self, a: usize, b: usize) -> bool {
        self.parent[b] = a;
        true
    }

    pub fn components(&self) -> usize {
        self.parent.iter().enumerate().filter(|(i, p)| i == *p).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_separate() {
        assert_eq!(DisjointSet::new(3).components(), 3);
    }
}
