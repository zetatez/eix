use inventory::{build_report, parse};

#[test]
fn duplicates_sum_and_names_trim() {
    let items = parse(" apple = 2 \napple=3\n\n pear = 1\n").unwrap();
    assert_eq!(build_report(&items), "apple: 5\npear: 1");
}

#[test]
fn invalid_lines_are_errors() {
    assert!(parse("apple").is_err());
    assert!(parse("apple=x").is_err());
}

#[test]
fn empty_input_is_an_empty_report() {
    assert_eq!(build_report(&parse("").unwrap()), "");
}
