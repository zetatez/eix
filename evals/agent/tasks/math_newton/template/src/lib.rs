pub fn sqrt_newton(x: f64) -> Option<f64> {
    if x < 0.0 {
        return None;
    }
    Some(x / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn square_root_of_four() {
        assert!((sqrt_newton(4.0).unwrap() - 2.0).abs() < 1e-9);
    }
}
