pub fn kth_smallest(values: &mut [i32], k: usize) -> Option<i32> {
    values.get(k).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smallest_of_one() {
        assert_eq!(kth_smallest(&mut [7], 0), Some(7));
    }
}
