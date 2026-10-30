pub fn integrate<F: Fn(f64) -> f64>(f: F, a: f64, b: f64, n: u32) -> Option<f64> {
    if n == 0 {
        return None;
    }
    let h = (b - a) / n as f64;
    let mut total = 0.0;
    for index in 0..n {
        total += f(a + index as f64 * h);
    }
    Some(total * h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrates_a_constant() {
        assert_eq!(integrate(|_| 2.0, 0.0, 1.0, 2), Some(2.0));
    }
}
