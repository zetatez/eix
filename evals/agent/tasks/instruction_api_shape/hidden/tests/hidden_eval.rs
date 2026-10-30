use instr_clamp::{clamp01, clamp_percent};

#[test]
fn clamps_percent() {
    assert_eq!(clamp_percent(-5), 0);
    assert_eq!(clamp_percent(0), 0);
    assert_eq!(clamp_percent(42), 42);
    assert_eq!(clamp_percent(100), 100);
    assert_eq!(clamp_percent(101), 100);
}

#[test]
fn clamp01_is_unchanged() {
    assert_eq!(clamp01(0.5), 0.5);
    assert_eq!(clamp01(-1.0), 0.0);
    assert_eq!(clamp01(2.0), 1.0);
}
