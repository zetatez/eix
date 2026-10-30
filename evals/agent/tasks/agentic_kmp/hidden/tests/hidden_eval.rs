use kmp::find_all;

#[test]
fn finds_overlapping_matches() {
    assert_eq!(find_all("aaaa", "aa"), vec![0, 1, 2]);
    assert_eq!(find_all("abababa", "aba"), vec![0, 2, 4]);
}

#[test]
fn handles_unicode_and_missing() {
    assert_eq!(find_all("éé", "é"), vec![0, 2]);
    assert_eq!(find_all("abc", "d"), Vec::<usize>::new());
    assert_eq!(find_all("abc", ""), Vec::<usize>::new());
    assert_eq!(find_all("aaaa", "aaaaa"), Vec::<usize>::new());
}
