use dijkstra::shortest;

#[test]
fn prefers_cheaper_detours() {
    let graph = [(0, 1, 1), (0, 2, 4), (1, 2, 1), (2, 3, 1)];
    let result = shortest(4, &graph, 0).unwrap();
    assert_eq!(result, vec![Some(0), Some(1), Some(2), Some(3)]);
}

#[test]
fn marks_unreachable_nodes() {
    let result = shortest(3, &[(0, 1, 7)], 0).unwrap();
    assert_eq!(result, vec![Some(0), Some(7), None]);
    assert_eq!(shortest(3, &[(0, 1, 7)], 3), None);
}

#[test]
fn validates_endpoints_and_duplicates() {
    assert_eq!(shortest(2, &[(0, 2, 1)], 0), None);
    let result = shortest(2, &[(0, 1, 9), (0, 1, 3)], 0).unwrap();
    assert_eq!(result[1], Some(3));
}
