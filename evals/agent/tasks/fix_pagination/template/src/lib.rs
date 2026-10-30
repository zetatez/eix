/// Return one page of `items`.
///
/// Pages are 1-based. The last page may contain fewer than `per_page` items.
pub fn paginate(items: &[i32], page: usize, per_page: usize) -> &[i32] {
    let start = page * per_page;
    let end = start + per_page;
    &items[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_page_is_one_based() {
        assert_eq!(paginate(&[1, 2, 3, 4, 5], 1, 2), &[1, 2]);
    }

    #[test]
    fn last_page_is_partial() {
        assert_eq!(paginate(&[1, 2, 3, 4, 5], 3, 2), &[5]);
    }
}
