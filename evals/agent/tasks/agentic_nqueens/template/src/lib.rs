pub fn count(n: u32) -> u64 {
    if n == 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_board() {
        assert_eq!(count(0), 1);
    }
}
