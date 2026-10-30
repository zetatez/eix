pub fn factorize(mut n: u64) -> Vec<(u64, u32)> {
    let mut out = Vec::new();
    let mut divisor = 2u64;
    while divisor * divisor <= n {
        if n % divisor == 0 {
            out.push((divisor, 1));
            n /= divisor;
        } else {
            divisor += 1;
        }
    }
    if n > 1 {
        out.push((n, 1));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factors_two() {
        assert_eq!(factorize(2), vec![(2, 1)]);
    }
}
