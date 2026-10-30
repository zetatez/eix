use utf8_truncate::truncate;

#[test]
fn keeps_within_budget() {
    assert_eq!(truncate("hello world", 8, "..."), "hello...");
    assert_eq!(truncate("hi", 8, "..."), "hi");
    assert_eq!(truncate("abc", 3, "..."), "abc");
    assert_eq!(truncate("abcdef", 2, "..."), "..");
}

#[test]
fn respects_char_boundaries() {
    assert_eq!(truncate("你好世界", 7, ".."), "你..");
    assert_eq!(truncate("aéx", 3, ""), "aé");
    assert_eq!(truncate("aéx", 2, ""), "a");
    assert!(truncate("日本語", 4, "-").len() <= 4);
}
