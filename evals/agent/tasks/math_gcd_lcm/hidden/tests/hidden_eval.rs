use numtheory::{gcd, lcm};

#[test]
fn zero_cases() {
    assert_eq!(gcd(0, 0), 0);
    assert_eq!(gcd(0, 5), 5);
    assert_eq!(gcd(5, 0), 5);
    assert_eq!(lcm(0, 5), Some(0));
    assert_eq!(lcm(5, 0), Some(0));
}

#[test]
fn signs_do_not_matter() {
    assert_eq!(gcd(-4, 6), 2);
    assert_eq!(gcd(4, -6), 2);
    assert_eq!(gcd(-4, -6), 2);
    assert_eq!(lcm(-4, 6), Some(12));
    assert_eq!(lcm(4, -6), Some(12));
}

#[test]
fn handles_i64_min() {
    assert_eq!(gcd(i64::MIN, 2), 2);
    assert_eq!(gcd(i64::MIN, i64::MIN), 1u64 << 63);
}

#[test]
fn lcm_reports_overflow() {
    assert_eq!(lcm(i64::MAX, i64::MAX - 2), None);
}

#[test]
fn lcm_of_coprime_values() {
    assert_eq!(lcm(21, 20), Some(420));
}
