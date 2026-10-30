pub struct Bucket {
    capacity: u64,
    rate: u64,
    tokens: u64,
    last: u64,
}

impl Bucket {
    pub fn new(capacity: u64, rate: u64) -> Self {
        Self { capacity, rate, tokens: capacity, last: 0 }
    }

    pub fn try_take(&mut self, now: u64) -> bool {
        self.last = now;
        if self.tokens > 0 {
            self.tokens -= 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_full() {
        let mut bucket = Bucket::new(2, 1);
        assert!(bucket.try_take(0));
    }
}
