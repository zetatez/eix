pub mod api;
pub mod report;

pub fn total(values: &[i64]) -> i64 {
    api::old_name(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals() {
        assert_eq!(total(&[1, 2, 3]), 6);
    }
}
