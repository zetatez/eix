use radix::{from_base, to_base};

#[test]
fn formats_bases() {
    assert_eq!(to_base(255, 16), Some("ff".to_string()));
    assert_eq!(to_base(0, 2), Some("0".to_string()));
    assert_eq!(to_base(35, 36), Some("z".to_string()));
    assert_eq!(to_base(5, 1), None);
    assert_eq!(to_base(5, 37), None);
}

#[test]
fn parses_bases() {
    assert_eq!(from_base("FF", 16), Some(255));
    assert_eq!(from_base("z", 36), Some(35));
    assert_eq!(from_base("2", 2), None);
    assert_eq!(from_base("", 10), None);
    assert_eq!(from_base("1", 1), None);
}
