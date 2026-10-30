use std::collections::BTreeSet;

pub fn order(nodes: usize, edges: &[(usize, usize)]) -> Option<Vec<usize>> {
    let mut incoming = vec![0usize; nodes];
    for &(_, to) in edges {
        incoming[to] += 1;
    }
    let mut ready: BTreeSet<usize> = (0..nodes).filter(|n| incoming[*n] == 0).collect();
    let mut out = Vec::with_capacity(nodes);
    while let Some(node) = ready.pop_first() {
        out.push(node);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_every_node() {
        let result = order(3, &[(0, 1)]).unwrap();
        assert_eq!(result.len(), 3);
    }
}
