use cli_args::{parse, Args};

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn parses_all_flags() {
    let args = parse(&strings(&["--verbose", "--name=alice", "--count=3"])).unwrap();
    assert_eq!(
        args,
        Args { name: Some("alice".to_string()), verbose: true, count: 3 }
    );
    assert_eq!(parse(&[]).unwrap(), Args::default());
}

#[test]
fn reports_errors() {
    assert_eq!(parse(&strings(&["--count=x"])).unwrap_err(), "invalid count: x");
    assert_eq!(parse(&strings(&["--name"])).unwrap_err(), "missing value: --name");
    assert_eq!(parse(&strings(&["--count"])).unwrap_err(), "missing value: --count");
    assert_eq!(parse(&strings(&["--fast"])).unwrap_err(), "unknown: --fast");
}

#[test]
fn last_value_wins() {
    let args = parse(&strings(&["--name=a", "--name=b", "--count=1", "--count=2"])).unwrap();
    assert_eq!(args.name.as_deref(), Some("b"));
    assert_eq!(args.count, 2);
}
