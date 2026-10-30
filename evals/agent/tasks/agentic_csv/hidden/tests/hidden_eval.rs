use csvlite::{parse, split_line};

#[test]
fn plain_and_quoted_fields() {
    assert_eq!(split_line("a,b,c"), vec!["a", "b", "c"]);
    assert_eq!(split_line("a,\"b,c\",d"), vec!["a", "b,c", "d"]);
}

#[test]
fn escaped_quotes() {
    assert_eq!(split_line("\"a\"\"b\",c"), vec!["a\"b", "c"]);
}

#[test]
fn parses_lines() {
    assert_eq!(
        parse("a,b\nc,\"d,e\""),
        vec![
            vec!["a".to_string(), "b".to_string()],
            vec!["c".to_string(), "d,e".to_string()],
        ]
    );
    assert_eq!(parse(""), Vec::<Vec<String>>::new());
}
