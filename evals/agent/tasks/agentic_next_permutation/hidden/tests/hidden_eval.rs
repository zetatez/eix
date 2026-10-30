use next_permutation::next_permutation;

#[test]
fn advances_lexicographically() {
    let mut values = [1, 2, 3];
    assert!(next_permutation(&mut values));
    assert_eq!(values, [1, 3, 2]);
    assert!(next_permutation(&mut values));
    assert_eq!(values, [2, 1, 3]);
}

#[test]
fn wraps_at_the_end() {
    let mut values = [3, 2, 1];
    assert!(!next_permutation(&mut values));
    assert_eq!(values, [1, 2, 3]);
    let mut empty: [i32; 0] = [];
    assert!(!next_permutation(&mut empty));
}

#[test]
fn handles_duplicates() {
    let mut values = [1, 1, 2];
    assert!(next_permutation(&mut values));
    assert_eq!(values, [1, 2, 1]);
    assert!(next_permutation(&mut values));
    assert_eq!(values, [2, 1, 1]);
    assert!(!next_permutation(&mut values));
    assert_eq!(values, [1, 1, 2]);
}
