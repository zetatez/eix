pub fn union(a: &[i32], b: &[i32]) -> Vec<i32> {
    let mut out = a.to_vec();
    out.extend_from_slice(b);
    out
}

pub fn intersection(a: &[i32], _b: &[i32]) -> Vec<i32> {
    a.to_vec()
}

pub fn difference(a: &[i32], _b: &[i32]) -> Vec<i32> {
    a.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn union_keeps_values() {
        assert!(union(&[1], &[2]).contains(&2));
    }
}
