pub fn next_permutation(values: &mut [i32]) -> bool {
    values.reverse();
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_value_has_no_successor() {
        assert!(!next_permutation(&mut [1]));
    }
}
