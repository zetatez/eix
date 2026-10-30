use std::collections::VecDeque;

pub struct RingBuffer {
    capacity: usize,
    items: VecDeque<i32>,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity, items: VecDeque::new() }
    }

    pub fn push(&mut self, value: i32) {
        self.items.push_back(value);
    }

    pub fn iter(&self) -> impl Iterator<Item = &i32> {
        self.items.iter()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_values() {
        let mut buffer = RingBuffer::new(2);
        buffer.push(1);
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.iter().copied().collect::<Vec<_>>(), vec![1]);
    }
}
