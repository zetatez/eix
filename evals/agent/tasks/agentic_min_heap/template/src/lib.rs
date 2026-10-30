#[derive(Default)]
pub struct MinHeap {
    items: Vec<i32>,
}

impl MinHeap {
    pub fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn push(&mut self, value: i32) {
        self.items.push(value);
    }

    pub fn pop(&mut self) -> Option<i32> {
        self.items.pop()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pushes_and_pops() {
        let mut heap = MinHeap::new();
        heap.push(1);
        assert_eq!(heap.pop(), Some(1));
    }
}
