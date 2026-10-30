use std::collections::BTreeMap;

use crate::Item;

/// Render one `name: qty` line per item, sorted by name.
pub fn build_report(items: &[Item]) -> String {
    let mut totals = BTreeMap::new();
    for item in items {
        totals.insert(item.name.clone(), item.qty);
    }
    totals
        .into_iter()
        .map(|(name, qty)| format!("{name}: {qty}"))
        .collect::<Vec<_>>()
        .join("\n")
}
