pub fn merge(intervals: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = Vec::new();
    for &(start, end) in intervals {
        if let Some(last) = out.last_mut() {
            if start < last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        out.push((start, end));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_overlapping() {
        assert_eq!(merge(&[(1, 3), (2, 6)]), vec![(1, 6)]);
    }
}
