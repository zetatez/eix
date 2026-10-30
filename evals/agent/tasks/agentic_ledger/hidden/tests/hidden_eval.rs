use ledger::{entry::{parse_entry, Entry}, ledger::apply};

#[test]
fn applies_mixed_entries() {
    assert_eq!(apply(&["deposit:10", "withdraw:3", "deposit:5"]), Ok(12));
    assert_eq!(apply(&[]), Ok(0));
}

#[test]
fn rejects_overdraft() {
    assert_eq!(apply(&["withdraw:1"]).unwrap_err(), "overdraft: withdraw:1");
    assert_eq!(apply(&["deposit:2", "withdraw:5"]).unwrap_err(), "overdraft: withdraw:5");
}

#[test]
fn rejects_malformed_entries() {
    assert_eq!(apply(&["nope"]).unwrap_err(), "bad entry: nope");
    assert_eq!(apply(&["deposit:+2"]).unwrap_err(), "bad entry: deposit:+2");
    assert_eq!(apply(&["deposit:0"]).unwrap_err(), "bad entry: deposit:0");
    assert_eq!(apply(&["withdraw:-1"]).unwrap_err(), "bad entry: withdraw:-1");
}

#[test]
fn parses_entries() {
    assert_eq!(parse_entry("deposit:5"), Some(Entry::Deposit(5)));
    assert_eq!(parse_entry("withdraw:2"), Some(Entry::Withdraw(2)));
    assert_eq!(parse_entry("transfer:2"), None);
    assert_eq!(parse_entry("deposit:2:3"), None);
}
