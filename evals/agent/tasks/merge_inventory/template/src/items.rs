#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub name: String,
    pub qty: u32,
}

/// Parse `name=qty` lines. Blank lines are ignored.
pub fn parse(input: &str) -> Result<Vec<Item>, String> {
    input
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (name, qty) = line
                .split_once('=')
                .ok_or_else(|| format!("bad line: {line}"))?;
            let qty = qty
                .parse::<u32>()
                .map_err(|_| format!("bad quantity: {line}"))?;
            Ok(Item {
                name: name.to_string(),
                qty,
            })
        })
        .collect()
}
