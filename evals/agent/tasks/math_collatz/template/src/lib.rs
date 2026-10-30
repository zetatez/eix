pub fn steps(mut n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    let mut count = 0;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        count += 1;
    }
    Some(count)
}

pub fn max_value(mut n: u64) -> Option<u64> {
    if n == 0 {
        return None;
    }
    let mut highest = n;
    while n != 1 {
        n = if n % 2 == 0 { n / 2 } else { 3 * n + 1 };
        highest = highest.max(n);
    }
    Some(highest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_needs_no_steps() {
        assert_eq!(steps(1), Some(0));
    }
}
