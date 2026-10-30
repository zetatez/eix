pub mod entry;
pub mod ledger;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_deposits() {
        assert_eq!(ledger::apply(&["deposit:5"]), Ok(5));
    }
}
