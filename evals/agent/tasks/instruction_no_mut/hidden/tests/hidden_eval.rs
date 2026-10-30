use instr_nomut::sum;

#[test]
fn sums_values() {
    assert_eq!(sum(&[]), 0);
    assert_eq!(sum(&[1, 2, 3]), 6);
    assert_eq!(sum(&[-4, 4]), 0);
}
