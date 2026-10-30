#[derive(Debug, PartialEq, Eq)]
pub enum Entry {
    Deposit(i64),
    Withdraw(i64),
}

pub fn parse_entry(line: &str) -> Option<Entry> {
    let parts: Vec<&str> = line.split(':').collect();
    let amount: i64 = parts.get(1)?.parse().ok()?;
    Some(Entry::Deposit(amount))
}
