use crt::crt;

#[test]
fn combines_coprime_moduli() {
    assert_eq!(crt(&[2, 3], &[3, 5]), Some(8));
    assert_eq!(crt(&[1, 2], &[3, 5]), Some(7));
    assert_eq!(crt(&[0, 0], &[2, 3]), Some(0));
    assert_eq!(crt(&[-1], &[5]), Some(4));
}

#[test]
fn handles_degenerate_input() {
    assert_eq!(crt(&[], &[]), Some(0));
    assert_eq!(crt(&[1], &[]), None);
    assert_eq!(crt(&[1, 2], &[2, 4]), None);
    assert_eq!(crt(&[1], &[0]), None);
    assert_eq!(crt(&[1], &[-3]), None);
}
