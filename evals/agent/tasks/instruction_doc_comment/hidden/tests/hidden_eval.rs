use instr_doc::square;

#[test]
fn square_still_works() {
    assert_eq!(square(0), 0);
    assert_eq!(square(5), 25);
}
