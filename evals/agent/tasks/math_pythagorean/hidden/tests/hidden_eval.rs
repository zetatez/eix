use pythagorean::primitive_triples;

#[test]
fn lists_triples_under_thirty() {
    assert_eq!(
        primitive_triples(30),
        vec![(3, 4, 5), (5, 12, 13), (8, 15, 17), (7, 24, 25), (20, 21, 29)]
    );
}

#[test]
fn skips_non_primitive_and_respects_limit() {
    let triples = primitive_triples(100);
    assert_eq!(triples.len(), 16);
    assert!(triples.contains(&(3, 4, 5)));
    assert!(!triples.contains(&(6, 8, 10)));
    assert!(triples.iter().all(|&(a, b, c)| a < b && c <= 100 && a * a + b * b == c * c));
    assert!(primitive_triples(4).is_empty());
}
