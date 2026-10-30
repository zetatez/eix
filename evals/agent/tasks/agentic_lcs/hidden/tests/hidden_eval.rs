use lcs::lcs;

fn is_subsequence(candidate: &str, source: &str) -> bool {
    let mut chars = source.chars();
    candidate.chars().all(|ch| chars.any(|other| other == ch))
}

#[test]
fn finds_a_longest_common_subsequence() {
    let result = lcs("ABCBDAB", "BDCABA");
    assert_eq!(result.chars().count(), 4);
    assert!(is_subsequence(&result, "ABCBDAB"));
    assert!(is_subsequence(&result, "BDCABA"));

    let result = lcs("AGGTAB", "GXTXAYB");
    assert_eq!(result.chars().count(), 4);
    assert!(is_subsequence(&result, "AGGTAB"));
}

#[test]
fn handles_empty_and_identical() {
    assert_eq!(lcs("", "abc"), "");
    assert_eq!(lcs("abc", ""), "");
    assert_eq!(lcs("abc", "abc"), "abc");
}
