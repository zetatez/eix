pub fn shortest(
    nodes: usize,
    edges: &[(usize, usize, u64)],
    source: usize,
) -> Option<Vec<Option<u64>>> {
    if source >= nodes {
        return None;
    }
    let mut distance = vec![None; nodes];
    distance[source] = Some(0);
    let mut queue = vec![source];
    while let Some(node) = queue.pop() {
        for &(from, to, _) in edges {
            if from == node && distance[to].is_none() {
                distance[to] = Some(distance[node].unwrap() + 1);
                queue.push(to);
            }
        }
    }
    Some(distance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_edge() {
        let result = shortest(2, &[(0, 1, 5)], 0).unwrap();
        assert_eq!(result, vec![Some(0), Some(5)]);
    }
}
