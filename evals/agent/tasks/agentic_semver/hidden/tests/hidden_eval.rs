use semver::Version;

#[test]
fn rejects_malformed_versions() {
    assert_eq!(Version::parse("1.2"), None);
    assert_eq!(Version::parse("1.2.3.4"), None);
    assert_eq!(Version::parse("1..3"), None);
    assert_eq!(Version::parse("01.2.3"), None);
    assert_eq!(Version::parse("1.2.x"), None);
    assert_eq!(Version::parse(""), None);
}

#[test]
fn sorts_numerically() {
    let a = Version::parse("1.2.3").unwrap();
    let b = Version::parse("1.10.0").unwrap();
    let c = Version::parse("2.0.0").unwrap();
    assert!(a < b);
    assert!(b < c);
    assert_eq!(Version::parse("0.0.0").unwrap().major, 0);
}
