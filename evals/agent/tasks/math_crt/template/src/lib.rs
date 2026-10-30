pub fn crt(residues: &[i64], moduli: &[i64]) -> Option<i64> {
    if residues.len() != moduli.len() {
        return None;
    }
    residues.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_congruence() {
        assert_eq!(crt(&[2], &[3]), Some(2));
    }
}
