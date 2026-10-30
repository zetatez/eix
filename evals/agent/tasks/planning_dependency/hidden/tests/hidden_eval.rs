use plan_pipeline2::{parse, summarize};

#[test]
fn parses_valid_lines() {
    assert_eq!(parse("a:1\nb:2"), vec![("a".into(), 1), ("b".into(), 2)]);
    assert_eq!(parse("a:1\nbad\nc:-3"), vec![("a".into(), 1), ("c".into(), -3)]);
    assert_eq!(parse(" :1\nx:y"), Vec::<(String, i64)>::new());
}

#[test]
fn summarizes() {
    assert_eq!(summarize("a:1\nb:2\nbad"), "a=1, b=2, total=3");
    assert_eq!(summarize(""), "total=0");
}
