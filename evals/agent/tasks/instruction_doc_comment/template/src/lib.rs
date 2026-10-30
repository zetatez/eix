#![deny(missing_docs)]
//! Small helper crate.

pub fn square(x: i64) -> i64 {
    x * x
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squares() {
        assert_eq!(square(4), 16);
    }
}
