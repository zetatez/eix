use roman::{from_roman, to_roman};

#[test]
fn uses_subtractive_notation() {
    let cases = [
        (4, "IV"), (9, "IX"), (40, "XL"), (90, "XC"),
        (400, "CD"), (900, "CM"), (1994, "MCMXCIV"), (3999, "MMMCMXCIX"),
    ];
    for (value, numeral) in cases {
        assert_eq!(to_roman(value), Some(numeral.to_string()));
    }
    assert_eq!(to_roman(0), None);
    assert_eq!(to_roman(4000), None);
}

#[test]
fn parses_canonical_numerals() {
    assert_eq!(from_roman("MCMXCIV"), Some(1994));
    assert_eq!(from_roman("IV"), Some(4));
}

#[test]
fn rejects_non_canonical_numerals() {
    assert_eq!(from_roman(""), None);
    assert_eq!(from_roman("IIII"), None);
    assert_eq!(from_roman("IC"), None);
    assert_eq!(from_roman("ABC"), None);
}
