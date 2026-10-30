use crate::entry::parse_entry;

pub fn apply(lines: &[&str]) -> Result<i64, String> {
    let mut balance = 0;
    for line in lines {
        if let Some(entry) = parse_entry(line) {
            match entry {
                crate::entry::Entry::Deposit(amount) => balance += amount,
                crate::entry::Entry::Withdraw(amount) => balance += amount,
            }
        }
    }
    Ok(balance)
}
