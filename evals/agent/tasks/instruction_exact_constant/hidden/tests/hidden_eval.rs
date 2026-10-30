use instr_const::{DEFAULT_LIMIT, LIMIT};

#[test]
fn constants_have_exact_values() {
    assert_eq!(LIMIT, 4096);
    assert_eq!(DEFAULT_LIMIT, 1024);
}
