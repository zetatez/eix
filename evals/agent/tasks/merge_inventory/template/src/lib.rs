mod items;
mod report;

pub use items::{parse, Item};
pub use report::build_report;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_names_are_summed() {
        let items = parse("apple=2\napple=3\n").unwrap();
        assert_eq!(build_report(&items), "apple: 5");
    }
}
