use instr_fmt::format_duration;

#[test]
fn examples() {
    assert_eq!(format_duration(0), "0:00:00");
    assert_eq!(format_duration(59), "0:00:59");
    assert_eq!(format_duration(60), "0:01:00");
    assert_eq!(format_duration(3661), "1:01:01");
    assert_eq!(format_duration(360_000), "100:00:00");
}
