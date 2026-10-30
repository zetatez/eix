#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counter {
    value: i64,
}

impl Counter {
    pub fn new(value: i64) -> Self {
        Self { value }
    }

    pub fn value(&self) -> i64 {
        self.value
    }

    /// Add `delta`, saturating at `i64::MAX`.
    pub fn add(&mut self, delta: i64) -> i64 {
        self.value = self.value + delta;
        self.value
    }

    /// Subtract `delta`, saturating at `i64::MIN`.
    pub fn sub(&mut self, delta: i64) -> i64 {
        self.value = self.value - delta;
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_and_subtracts() {
        let mut counter = Counter::new(10);
        assert_eq!(counter.add(5), 15);
        assert_eq!(counter.sub(3), 12);
    }
}
