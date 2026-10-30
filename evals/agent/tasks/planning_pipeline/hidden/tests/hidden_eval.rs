use pipeline::{run, tokenize, transform};

#[test]
fn run_normalizes_a_sentence() {
    assert_eq!(run("The quick, brown fox!"), "quick brown fox");
    assert_eq!(run("  A  AN the "), "");
    assert_eq!(run(""), "");
}

#[test]
fn tokenize_trims_punctuation_and_lowercases() {
    assert_eq!(tokenize("Hi, there!"), vec!["hi", "there"]);
    assert_eq!(tokenize("  -- "), Vec::<String>::new());
}

#[test]
fn transform_drops_only_exact_stopwords() {
    assert_eq!(transform(vec!["the".into(), "cat".into()]), vec!["cat"]);
    assert_eq!(transform(vec!["then".into(), "an".into()]), vec!["then"]);
}
